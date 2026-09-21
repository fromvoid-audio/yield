//! Private macOS protected-capacity reservation and verification.
//!
//! This helper consumes an already owner-created writable file descriptor. It never accepts a
//! path, creates a namespace entry, or reopens a locator. The returned handle is deliberately
//! non-serializable and non-cloneable; it is only an observation that one physical allocation was
//! made and verified. It is not an admission claim or a durable recovery record.

use std::fmt;
use std::fs::File;

use thiserror::Error;

use crate::durable_capacity::{
    PROTECTED_FREE_SPACE_FLOOR, VolumeIdentity, descriptor_capacity_facts,
};
use crate::filesystem_identity::stable_filesystem_identity_from_open_file;

/// One descriptor-bound observation retained by a protected reserve handle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ReserveObservation {
    pub(super) identity: String,
    pub(super) volume: VolumeIdentity,
    pub(super) allocation_unit: u64,
    pub(super) length: u64,
    pub(super) allocated_bytes: u64,
    pub(super) regular: bool,
    pub(super) links: u64,
    pub(super) compressed: bool,
}

/// A verified, non-serializable protected reserve held by its original file descriptor.
#[derive(Debug)]
pub(super) struct ProtectedReserve {
    file: File,
    observation: ReserveObservation,
}

impl ProtectedReserve {
    /// Revalidate the retained descriptor and its protected allocation without reopening a path.
    pub(super) fn revalidate(&self) -> Result<(), ReserveFailureReason> {
        #[cfg(target_os = "macos")]
        {
            let observed = capture_observation(&self.file)?;
            return validate_verified_observation(&observed, &self.observation);
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err(ReserveFailureReason::Unsupported)
        }
    }

    pub(super) fn observation(&self) -> &ReserveObservation {
        &self.observation
    }

    /// Release only the retained descriptor for caller-directed recovery.
    pub(super) fn into_file(self) -> File {
        self.file
    }
}

/// Failure stages for a reserve attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ReserveFailureStage {
    /// The target has no qualified implementation.
    Unsupported,
    /// The supplied descriptor did not meet the fresh-file preconditions.
    InitialValidation,
    /// The physical preallocation call failed or returned too little space.
    Preallocate,
    /// Extending the logical file length failed.
    SetLength,
    /// The required durable file flush failed.
    Sync,
    /// The post-allocation descriptor observation failed validation.
    Verification,
}

/// Typed reserve failure reason. The failed value retains the original owned file.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(super) enum ReserveFailureReason {
    #[error("protected reserve allocation is unsupported on this target")]
    Unsupported,
    #[error("reserve descriptor is not a regular file")]
    NotRegular,
    #[error("reserve descriptor is not writable")]
    NotWritable,
    #[error("reserve descriptor is not fresh: length {length}, allocated {allocated_bytes}")]
    NotFresh { length: u64, allocated_bytes: u64 },
    #[error("reserve descriptor does not have one hard link: {links}")]
    NotSingleLink { links: u64 },
    #[error("reserve descriptor is compressed")]
    Compressed,
    #[error("reserve descriptor identity is unavailable")]
    IdentityUnavailable,
    #[error("reserve volume facts are unavailable: {0}")]
    CapacityFactsUnavailable(String),
    #[error("reserve descriptor volume identity does not match its capacity facts")]
    VolumeMismatch,
    #[error("reserve descriptor allocation unit is zero")]
    ZeroAllocationUnit,
    #[error("reserve preallocation returned {allocated} bytes, requested {requested}")]
    ShortAllocation { requested: u64, allocated: u64 },
    #[error("reserve preallocation arithmetic overflowed")]
    ArithmeticOverflow,
    #[error("reserve operation failed: {0}")]
    Io(String),
    #[error("verified reserve identity changed")]
    IdentityMismatch,
    #[error("verified reserve volume changed")]
    VerifiedVolumeMismatch,
    #[error("verified reserve allocation unit changed")]
    VerifiedAllocationUnitMismatch,
    #[error("verified reserve length is {actual}, expected {expected}")]
    LengthMismatch { expected: u64, actual: u64 },
    #[error("verified reserve allocation is {actual}, below required {required}")]
    AllocationBelowFloor { required: u64, actual: u64 },
}

/// A reserve failure that owns the original descriptor for caller-directed recovery.
#[derive(Debug)]
pub(super) struct ReserveFailure {
    file: File,
    stage: ReserveFailureStage,
    reason: ReserveFailureReason,
    observed_allocation: Option<u64>,
}

impl ReserveFailure {
    fn new(
        file: File,
        stage: ReserveFailureStage,
        reason: ReserveFailureReason,
        observed_allocation: Option<u64>,
    ) -> Self {
        Self {
            file,
            stage,
            reason,
            observed_allocation,
        }
    }

    /// Recover the original owned descriptor without unlinking or truncating it.
    pub(super) fn into_file(self) -> File {
        self.file
    }

    pub(super) fn stage(&self) -> ReserveFailureStage {
        self.stage
    }

    pub(super) fn reason(&self) -> &ReserveFailureReason {
        &self.reason
    }

    pub(super) fn observed_allocation(&self) -> Option<u64> {
        self.observed_allocation
    }
}

impl fmt::Display for ReserveFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "protected reserve {:?}: {}",
            self.stage, self.reason
        )
    }
}

impl std::error::Error for ReserveFailure {}

/// Allocate and verify the fixed protected floor on an already owner-created file.
pub(super) fn allocate_protected_reserve(file: File) -> Result<ProtectedReserve, ReserveFailure> {
    #[cfg(target_os = "macos")]
    {
        return reserve_with_backend(file, MacReserveBackend);
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(ReserveFailure::new(
            file,
            ReserveFailureStage::Unsupported,
            ReserveFailureReason::Unsupported,
            None,
        ))
    }
}

/// Verify an already allocated reserve descriptor without performing any filesystem mutation.
///
/// The expected observation is only a comparison value. The returned reserve is constructed
/// from facts captured from `file`, so a stored observation cannot authorize a reserve by itself.
pub(super) fn revalidate_existing_owned_file(
    file: File,
    expected: &ReserveObservation,
) -> Result<ProtectedReserve, ReserveFailure> {
    #[cfg(target_os = "macos")]
    {
        let observed = match capture_observation(&file) {
            Ok(observation) => observation,
            Err(reason) => {
                return Err(ReserveFailure::new(
                    file,
                    ReserveFailureStage::InitialValidation,
                    reason,
                    None,
                ));
            }
        };
        if let Err(reason) = validate_verified_observation(&observed, expected) {
            return Err(ReserveFailure::new(
                file,
                ReserveFailureStage::Verification,
                reason,
                Some(observed.allocated_bytes),
            ));
        }
        return Ok(ProtectedReserve {
            file,
            observation: observed,
        });
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = expected;
        Err(ReserveFailure::new(
            file,
            ReserveFailureStage::Unsupported,
            ReserveFailureReason::Unsupported,
            None,
        ))
    }
}

#[cfg(test)]
pub(super) fn test_fixture_reserve(file: File) -> ProtectedReserve {
    let mut observation = capture_observation(&file).expect("test fixture descriptor facts");
    observation.length = PROTECTED_FREE_SPACE_FLOOR;
    observation.allocated_bytes = PROTECTED_FREE_SPACE_FLOOR;
    ProtectedReserve { file, observation }
}

trait ReserveBackend {
    fn preallocate(&self, file: &File, bytes: u64) -> Result<u64, ReserveFailureReason>;
    fn set_len(&self, file: &File, bytes: u64) -> Result<(), ReserveFailureReason>;
    fn sync(&self, file: &File) -> Result<(), ReserveFailureReason>;
    fn final_observation(&self, file: &File) -> Result<ReserveObservation, ReserveFailureReason>;
}

fn reserve_with_backend<B: ReserveBackend>(
    file: File,
    backend: B,
) -> Result<ProtectedReserve, ReserveFailure> {
    let initial = match capture_observation(&file) {
        Ok(observation) => observation,
        Err(reason) => {
            return Err(ReserveFailure::new(
                file,
                ReserveFailureStage::InitialValidation,
                reason,
                None,
            ));
        }
    };
    if let Err(reason) = validate_fresh_observation(&initial) {
        return Err(ReserveFailure::new(
            file,
            ReserveFailureStage::InitialValidation,
            reason,
            Some(initial.allocated_bytes),
        ));
    }
    if let Err(reason) = validate_writable(&file) {
        return Err(ReserveFailure::new(
            file,
            ReserveFailureStage::InitialValidation,
            reason,
            Some(initial.allocated_bytes),
        ));
    }

    let allocated = match backend.preallocate(&file, PROTECTED_FREE_SPACE_FLOOR) {
        Ok(allocated) => allocated,
        Err(reason) => {
            let observed_allocation = capture_observation(&file)
                .ok()
                .map(|observation| observation.allocated_bytes);
            return Err(ReserveFailure::new(
                file,
                ReserveFailureStage::Preallocate,
                reason,
                observed_allocation,
            ));
        }
    };
    if allocated < PROTECTED_FREE_SPACE_FLOOR {
        let observed_allocation = capture_observation(&file)
            .ok()
            .map(|observation| observation.allocated_bytes);
        return Err(ReserveFailure::new(
            file,
            ReserveFailureStage::Preallocate,
            ReserveFailureReason::ShortAllocation {
                requested: PROTECTED_FREE_SPACE_FLOOR,
                allocated,
            },
            observed_allocation,
        ));
    }

    if let Err(reason) = backend.set_len(&file, PROTECTED_FREE_SPACE_FLOOR) {
        let observed_allocation = capture_observation(&file)
            .ok()
            .map(|observation| observation.allocated_bytes);
        return Err(ReserveFailure::new(
            file,
            ReserveFailureStage::SetLength,
            reason,
            observed_allocation,
        ));
    }
    if let Err(reason) = backend.sync(&file) {
        let observed_allocation = capture_observation(&file)
            .ok()
            .map(|observation| observation.allocated_bytes);
        return Err(ReserveFailure::new(
            file,
            ReserveFailureStage::Sync,
            reason,
            observed_allocation,
        ));
    }

    let observed = match backend.final_observation(&file) {
        Ok(observed) => observed,
        Err(reason) => {
            let observed_allocation = capture_observation(&file)
                .ok()
                .map(|observation| observation.allocated_bytes);
            return Err(ReserveFailure::new(
                file,
                ReserveFailureStage::Verification,
                reason,
                observed_allocation,
            ));
        }
    };
    if let Err(reason) = validate_verified_observation(&observed, &initial) {
        return Err(ReserveFailure::new(
            file,
            ReserveFailureStage::Verification,
            reason,
            Some(observed.allocated_bytes),
        ));
    }
    Ok(ProtectedReserve {
        file,
        observation: observed,
    })
}

fn validate_fresh_observation(
    observation: &ReserveObservation,
) -> Result<(), ReserveFailureReason> {
    if !observation.regular {
        return Err(ReserveFailureReason::NotRegular);
    }
    if observation.links != 1 {
        return Err(ReserveFailureReason::NotSingleLink {
            links: observation.links,
        });
    }
    if observation.compressed {
        return Err(ReserveFailureReason::Compressed);
    }
    if observation.length != 0 || observation.allocated_bytes != 0 {
        return Err(ReserveFailureReason::NotFresh {
            length: observation.length,
            allocated_bytes: observation.allocated_bytes,
        });
    }
    if observation.allocation_unit == 0 {
        return Err(ReserveFailureReason::ZeroAllocationUnit);
    }
    Ok(())
}

fn validate_verified_observation(
    observation: &ReserveObservation,
    expected: &ReserveObservation,
) -> Result<(), ReserveFailureReason> {
    if !observation.regular {
        return Err(ReserveFailureReason::NotRegular);
    }
    if observation.links != 1 {
        return Err(ReserveFailureReason::NotSingleLink {
            links: observation.links,
        });
    }
    if observation.compressed {
        return Err(ReserveFailureReason::Compressed);
    }
    if observation.identity != expected.identity {
        return Err(ReserveFailureReason::IdentityMismatch);
    }
    if observation.volume != expected.volume {
        return Err(ReserveFailureReason::VerifiedVolumeMismatch);
    }
    if observation.allocation_unit != expected.allocation_unit {
        return Err(ReserveFailureReason::VerifiedAllocationUnitMismatch);
    }
    if observation.length != PROTECTED_FREE_SPACE_FLOOR {
        return Err(ReserveFailureReason::LengthMismatch {
            expected: PROTECTED_FREE_SPACE_FLOOR,
            actual: observation.length,
        });
    }
    let required_allocation = rounded_floor(expected.allocation_unit)?;
    if observation.allocated_bytes < required_allocation {
        return Err(ReserveFailureReason::AllocationBelowFloor {
            required: required_allocation,
            actual: observation.allocated_bytes,
        });
    }
    Ok(())
}

fn rounded_floor(allocation_unit: u64) -> Result<u64, ReserveFailureReason> {
    if allocation_unit == 0 {
        return Err(ReserveFailureReason::ZeroAllocationUnit);
    }
    let remainder = PROTECTED_FREE_SPACE_FLOOR % allocation_unit;
    if remainder == 0 {
        Ok(PROTECTED_FREE_SPACE_FLOOR)
    } else {
        PROTECTED_FREE_SPACE_FLOOR
            .checked_add(allocation_unit - remainder)
            .ok_or(ReserveFailureReason::ArithmeticOverflow)
    }
}

fn validate_writable(file: &File) -> Result<(), ReserveFailureReason> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;

        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        if flags == -1 {
            return Err(ReserveFailureReason::Io(
                std::io::Error::last_os_error().to_string(),
            ));
        }
        let access = flags & libc::O_ACCMODE;
        if access != libc::O_WRONLY && access != libc::O_RDWR {
            return Err(ReserveFailureReason::NotWritable);
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        Err(ReserveFailureReason::Unsupported)
    }
}

fn capture_observation(file: &File) -> Result<ReserveObservation, ReserveFailureReason> {
    let facts = descriptor_capacity_facts(file)
        .map_err(|error| ReserveFailureReason::CapacityFactsUnavailable(error.to_string()))?;
    if facts.allocation_unit == 0 {
        return Err(ReserveFailureReason::ZeroAllocationUnit);
    }
    let identity = stable_filesystem_identity_from_open_file(file)
        .ok_or(ReserveFailureReason::IdentityUnavailable)?;
    let raw = raw_file_observation(file)?;
    if facts.identity.device != raw.device {
        return Err(ReserveFailureReason::VolumeMismatch);
    }
    Ok(ReserveObservation {
        identity,
        volume: facts.identity,
        allocation_unit: facts.allocation_unit,
        length: raw.length,
        allocated_bytes: raw.allocated_bytes,
        regular: raw.regular,
        links: raw.links,
        compressed: raw.compressed,
    })
}

struct RawFileObservation {
    device: u64,
    length: u64,
    allocated_bytes: u64,
    links: u64,
    regular: bool,
    compressed: bool,
}

#[cfg(unix)]
fn raw_file_observation(file: &File) -> Result<RawFileObservation, ReserveFailureReason> {
    use std::os::fd::AsRawFd;

    let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
    if unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(ReserveFailureReason::Io(
            std::io::Error::last_os_error().to_string(),
        ));
    }
    let stat = unsafe { stat.assume_init() };
    let length =
        u64::try_from(stat.st_size).map_err(|_| ReserveFailureReason::ArithmeticOverflow)?;
    let blocks =
        u64::try_from(stat.st_blocks).map_err(|_| ReserveFailureReason::ArithmeticOverflow)?;
    let allocated_bytes = blocks
        .checked_mul(512)
        .ok_or(ReserveFailureReason::ArithmeticOverflow)?;
    let links =
        u64::try_from(stat.st_nlink).map_err(|_| ReserveFailureReason::ArithmeticOverflow)?;
    #[cfg(target_os = "macos")]
    let compressed = (stat.st_flags & libc::UF_COMPRESSED) != 0;
    #[cfg(not(target_os = "macos"))]
    let compressed = false;
    Ok(RawFileObservation {
        device: stat.st_dev as u64,
        length,
        allocated_bytes,
        links,
        regular: (stat.st_mode & libc::S_IFMT) == libc::S_IFREG,
        compressed,
    })
}

#[cfg(not(unix))]
fn raw_file_observation(_file: &File) -> Result<RawFileObservation, ReserveFailureReason> {
    Err(ReserveFailureReason::Unsupported)
}

#[cfg(target_os = "macos")]
struct MacReserveBackend;

#[cfg(target_os = "macos")]
impl ReserveBackend for MacReserveBackend {
    fn preallocate(&self, file: &File, bytes: u64) -> Result<u64, ReserveFailureReason> {
        mac_preallocate(file, bytes)
    }

    fn set_len(&self, file: &File, bytes: u64) -> Result<(), ReserveFailureReason> {
        file.set_len(bytes)
            .map_err(|error| ReserveFailureReason::Io(error.to_string()))
    }

    fn sync(&self, file: &File) -> Result<(), ReserveFailureReason> {
        use std::os::fd::AsRawFd;

        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } == -1 {
            return Err(ReserveFailureReason::Io(
                std::io::Error::last_os_error().to_string(),
            ));
        }
        Ok(())
    }

    fn final_observation(&self, file: &File) -> Result<ReserveObservation, ReserveFailureReason> {
        capture_observation(file)
    }
}

#[cfg(target_os = "macos")]
fn mac_preallocate(file: &File, bytes: u64) -> Result<u64, ReserveFailureReason> {
    use std::os::fd::AsRawFd;

    let length =
        libc::off_t::try_from(bytes).map_err(|_| ReserveFailureReason::ArithmeticOverflow)?;
    let mut store = libc::fstore_t {
        fst_flags: libc::F_ALLOCATEALL,
        fst_posmode: libc::F_PEOFPOSMODE,
        fst_offset: 0,
        fst_length: length,
        fst_bytesalloc: 0,
    };
    if unsafe {
        libc::fcntl(
            file.as_raw_fd(),
            libc::F_PREALLOCATE,
            &mut store as *mut libc::fstore_t,
        )
    } == -1
    {
        return Err(ReserveFailureReason::Io(
            std::io::Error::last_os_error().to_string(),
        ));
    }
    u64::try_from(store.fst_bytesalloc).map_err(|_| ReserveFailureReason::ArithmeticOverflow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;

    #[cfg(target_os = "macos")]
    use std::io::{Read, Seek, SeekFrom, Write};

    fn fresh_file(temp: &tempfile::TempDir, name: &str) -> (std::path::PathBuf, File) {
        let path = temp.path().join(name);
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .expect("create fresh reserve file");
        (path, file)
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_allocates_and_revalidates_one_protected_reserve() {
        let temp = tempfile::tempdir().expect("reserve tempdir");
        let (path, file) = fresh_file(&temp, "reserve");
        let expected_identity =
            stable_filesystem_identity_from_open_file(&file).expect("fresh descriptor identity");
        let reserve = allocate_protected_reserve(file).expect("allocate protected reserve");
        assert_eq!(reserve.observation().identity, expected_identity);
        assert_eq!(reserve.observation().length, PROTECTED_FREE_SPACE_FLOOR);
        assert!(reserve.observation().allocated_bytes >= PROTECTED_FREE_SPACE_FLOOR);
        assert!(reserve.observation().volume.device > 0);
        assert!(reserve.observation().allocation_unit > 0);
        reserve.revalidate().expect("initial revalidation");

        let moved = temp.path().join("reserve-moved");
        std::fs::rename(&path, &moved).expect("rename retained reserve");
        reserve
            .revalidate()
            .expect("path replacement cannot redirect retained descriptor");

        let external = OpenOptions::new()
            .write(true)
            .open(&moved)
            .expect("reopen reserve for fault");
        external.set_len(0).expect("truncate reserve for fault");
        assert!(matches!(
            reserve.revalidate(),
            Err(ReserveFailureReason::LengthMismatch { .. })
                | Err(ReserveFailureReason::AllocationBelowFloor { .. })
        ));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn unsupported_target_returns_owned_file_without_mutation() {
        let temp = tempfile::tempdir().expect("reserve tempdir");
        let (path, file) = fresh_file(&temp, "reserve");
        let before = stable_filesystem_identity_from_open_file(&file);
        let failure = allocate_protected_reserve(file).expect_err("unsupported target");
        assert_eq!(failure.stage(), ReserveFailureStage::Unsupported);
        let file = failure.into_file();
        assert_eq!(stable_filesystem_identity_from_open_file(&file), before);
        assert_eq!(file.metadata().expect("metadata").len(), 0);
        assert!(path.exists());
    }

    #[cfg(target_os = "macos")]
    #[derive(Clone, Copy, Debug)]
    enum Fault {
        Unsupported,
        PreallocateError,
        MarkerPreallocateError,
        ShortAllocation,
        SyncFailure,
        MarkerSyncFailure,
        InsufficientBlocks,
        IdentityMismatch,
    }

    #[cfg(target_os = "macos")]
    const FAULT_MARKER: &[u8] = b"reserve-partial-marker";

    #[cfg(target_os = "macos")]
    struct FaultAdapter {
        fault: Fault,
    }

    #[cfg(target_os = "macos")]
    impl ReserveBackend for FaultAdapter {
        fn preallocate(&self, file: &File, bytes: u64) -> Result<u64, ReserveFailureReason> {
            match self.fault {
                Fault::Unsupported => Err(ReserveFailureReason::Unsupported),
                Fault::PreallocateError => Err(ReserveFailureReason::Io(
                    "injected preallocation failure".to_owned(),
                )),
                Fault::MarkerPreallocateError => {
                    write_fault_marker(file)?;
                    Err(ReserveFailureReason::Io(
                        "injected partial preallocation failure".to_owned(),
                    ))
                }
                Fault::ShortAllocation => Ok(bytes - 1),
                Fault::SyncFailure
                | Fault::MarkerSyncFailure
                | Fault::InsufficientBlocks
                | Fault::IdentityMismatch => Ok(bytes),
            }
        }

        fn set_len(&self, _file: &File, _bytes: u64) -> Result<(), ReserveFailureReason> {
            Ok(())
        }

        fn sync(&self, file: &File) -> Result<(), ReserveFailureReason> {
            match self.fault {
                Fault::SyncFailure => {
                    Err(ReserveFailureReason::Io("injected sync failure".to_owned()))
                }
                Fault::MarkerSyncFailure => {
                    write_fault_marker(file)?;
                    Err(ReserveFailureReason::Io(
                        "injected partial sync failure".to_owned(),
                    ))
                }
                _ => Ok(()),
            }
        }

        fn final_observation(
            &self,
            file: &File,
        ) -> Result<ReserveObservation, ReserveFailureReason> {
            let mut observation = capture_observation(file)?;
            observation.length = PROTECTED_FREE_SPACE_FLOOR;
            observation.allocated_bytes = PROTECTED_FREE_SPACE_FLOOR;
            match self.fault {
                Fault::InsufficientBlocks => {
                    observation.allocated_bytes = PROTECTED_FREE_SPACE_FLOOR - 1;
                }
                Fault::IdentityMismatch => observation.identity = "injected-mismatch".to_owned(),
                _ => {}
            }
            Ok(observation)
        }
    }

    #[cfg(target_os = "macos")]
    fn write_fault_marker(mut file: &File) -> Result<(), ReserveFailureReason> {
        file.write_all(FAULT_MARKER)
            .map_err(|error| ReserveFailureReason::Io(error.to_string()))
    }

    #[cfg(target_os = "macos")]
    fn metadata_allocated_bytes(file: &File) -> u64 {
        use std::os::unix::fs::MetadataExt;

        file.metadata()
            .expect("fault metadata")
            .blocks()
            .checked_mul(512)
            .expect("fault allocation arithmetic")
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn injected_failures_return_the_original_file_without_cleanup() {
        let faults = [
            (Fault::Unsupported, ReserveFailureStage::Preallocate),
            (Fault::PreallocateError, ReserveFailureStage::Preallocate),
            (Fault::ShortAllocation, ReserveFailureStage::Preallocate),
            (Fault::SyncFailure, ReserveFailureStage::Sync),
            (Fault::InsufficientBlocks, ReserveFailureStage::Verification),
            (Fault::IdentityMismatch, ReserveFailureStage::Verification),
        ];
        for (index, (fault, stage)) in faults.into_iter().enumerate() {
            let temp = tempfile::tempdir().expect("fault tempdir");
            let (path, file) = fresh_file(&temp, &format!("fault-{index}"));
            let before = stable_filesystem_identity_from_open_file(&file);
            let failure = reserve_with_backend(file, FaultAdapter { fault })
                .expect_err("injected reserve failure");
            assert_eq!(failure.stage(), stage);
            assert!(failure.observed_allocation().is_some());
            let file = failure.into_file();
            assert_eq!(stable_filesystem_identity_from_open_file(&file), before);
            assert_eq!(file.metadata().expect("metadata").len(), 0);
            assert!(path.exists());
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn partial_failures_retain_post_action_file_and_metadata_observation() {
        let faults = [
            (
                Fault::MarkerPreallocateError,
                ReserveFailureStage::Preallocate,
            ),
            (Fault::MarkerSyncFailure, ReserveFailureStage::Sync),
        ];
        for (index, (fault, stage)) in faults.into_iter().enumerate() {
            let temp = tempfile::tempdir().expect("partial fault tempdir");
            let (path, file) = fresh_file(&temp, &format!("partial-{index}"));
            let before = stable_filesystem_identity_from_open_file(&file);
            let failure = reserve_with_backend(file, FaultAdapter { fault })
                .expect_err("partial reserve failure");
            assert_eq!(failure.stage(), stage);
            assert_eq!(
                failure.reason(),
                &ReserveFailureReason::Io(match fault {
                    Fault::MarkerPreallocateError =>
                        "injected partial preallocation failure".to_owned(),
                    Fault::MarkerSyncFailure => "injected partial sync failure".to_owned(),
                    _ => unreachable!("only marker faults are tested here"),
                })
            );
            let expected_allocation = failure
                .observed_allocation()
                .expect("metadata allocation captured after partial work");
            let mut file = failure.into_file();
            let metadata = file.metadata().expect("retained metadata");
            assert_eq!(metadata.len(), FAULT_MARKER.len() as u64);
            assert_eq!(expected_allocation, metadata_allocated_bytes(&file));
            assert_eq!(stable_filesystem_identity_from_open_file(&file), before);
            file.seek(SeekFrom::Start(0)).expect("rewind retained file");
            let mut contents = Vec::new();
            file.read_to_end(&mut contents)
                .expect("read retained marker");
            assert_eq!(contents, FAULT_MARKER);
            assert!(path.exists());
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn invalid_nonempty_and_nonregular_inputs_return_unchanged_files() {
        let temp = tempfile::tempdir().expect("invalid tempdir");
        let (path, mut file) = fresh_file(&temp, "nonempty");
        std::io::Write::write_all(&mut file, b"occupied").expect("write invalid input");
        let before = stable_filesystem_identity_from_open_file(&file);
        let failure = allocate_protected_reserve(file).expect_err("nonempty input");
        assert_eq!(failure.stage(), ReserveFailureStage::InitialValidation);
        assert!(matches!(
            failure.reason(),
            ReserveFailureReason::NotFresh { .. }
        ));
        let file = failure.into_file();
        assert_eq!(stable_filesystem_identity_from_open_file(&file), before);
        assert_eq!(file.metadata().expect("metadata").len(), 8);
        assert!(path.exists());

        let directory = File::open(temp.path()).expect("open nonregular input");
        let failure = allocate_protected_reserve(directory).expect_err("directory input");
        assert_eq!(failure.stage(), ReserveFailureStage::InitialValidation);
        assert_eq!(failure.reason(), &ReserveFailureReason::NotRegular);
        let directory = failure.into_file();
        assert!(directory.metadata().expect("directory metadata").is_dir());

        let (readonly_path, writable) = fresh_file(&temp, "readonly");
        let before = stable_filesystem_identity_from_open_file(&writable);
        drop(writable);
        let readonly = OpenOptions::new()
            .read(true)
            .open(&readonly_path)
            .expect("reopen readonly input");
        let failure = allocate_protected_reserve(readonly).expect_err("readonly input");
        assert_eq!(failure.stage(), ReserveFailureStage::InitialValidation);
        assert_eq!(failure.reason(), &ReserveFailureReason::NotWritable);
        let readonly = failure.into_file();
        assert_eq!(stable_filesystem_identity_from_open_file(&readonly), before);
        assert_eq!(readonly.metadata().expect("readonly metadata").len(), 0);
        assert!(readonly_path.exists());

        let (hardlink_path, hardlink_file) = fresh_file(&temp, "hardlink");
        let alias_path = temp.path().join("hardlink-alias");
        std::fs::hard_link(&hardlink_path, &alias_path).expect("create hard link input");
        let before = stable_filesystem_identity_from_open_file(&hardlink_file);
        let failure = allocate_protected_reserve(hardlink_file).expect_err("hard link input");
        assert_eq!(failure.stage(), ReserveFailureStage::InitialValidation);
        assert_eq!(
            failure.reason(),
            &ReserveFailureReason::NotSingleLink { links: 2 }
        );
        let hardlink_file = failure.into_file();
        assert_eq!(
            stable_filesystem_identity_from_open_file(&hardlink_file),
            before
        );
        assert_eq!(
            hardlink_file.metadata().expect("hard link metadata").len(),
            0
        );
        assert!(hardlink_path.exists());
        assert!(alias_path.exists());
    }
}
