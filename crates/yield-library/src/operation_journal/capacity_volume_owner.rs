//! Private cross-profile ownership for one live filesystem volume.
//!
//! The owner is a per-user, descriptor-locked recovery capability. It is deliberately
//! independent of the selected persistence profile, and it does not authorize ordinary
//! admission, physical reserve spending, ledger transitions, or cleanup. A dirty marker is
//! durable evidence that the previous owner may have left work requiring reconciliation.

use std::{
    cell::Cell,
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    marker::PhantomData,
    path::{Path, PathBuf},
    rc::Rc,
};

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use thiserror::Error;

use crate::{
    app_dirs::{
        ProfileOwnershipError, WritableProfileGuard, open_canonical_global_app_root,
        open_no_follow_directory,
    },
    durable_capacity::{VolumeIdentity, descriptor_capacity_facts},
    filesystem_identity::stable_filesystem_identity_from_open_file,
};

const REGISTRY_DIRECTORY_NAME: &str = "volume-ownership";
const LOCK_FILE_PREFIX: &str = "volume-";
const LOCK_FILE_SUFFIX: &str = ".lock";
const MARKER_VERSION: &str = "1";
const MAX_MARKER_BYTES: usize = 1024;
const MAX_IDENTITY_BYTES: usize = 256;

#[cfg(any(windows, test))]
mod windows_lock;

mod legacy_bootstrap;
pub(super) use legacy_bootstrap::{LegacyVolumeFailureBacking, acquire_legacy_for_descriptor};

/// This capability can only be used for recovery coordination in this milestone.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum VolumeOwnershipReadiness {
    /// A later ledger and physical-proof step must reconcile the dirty owner marker first.
    RecoveryOnly,
}

/// Failure while establishing or validating the per-volume owner boundary.
#[derive(Debug, Error)]
pub(crate) enum VolumeOwnershipError {
    /// No identity-safe shared application root is qualified on this target.
    #[error("shared volume ownership is unsupported at {path}: {reason}")]
    Unsupported { path: PathBuf, reason: String },
    /// The shared registry root could not be opened or retained.
    #[error("shared volume ownership root unavailable at {path}: {reason}")]
    RootUnavailable { path: PathBuf, reason: String },
    /// The retained registry root no longer names the acquired directory.
    #[error("shared volume ownership root was replaced at {path}")]
    RootReplaced { path: PathBuf },
    /// The live descriptor did not yield a bounded volume identity.
    #[error("live volume identity is unavailable")]
    VolumeIdentityUnavailable,
    /// The volume identity cannot be represented by the fixed lock-entry name.
    #[error("live volume identity is invalid")]
    InvalidVolumeIdentity,
    /// The profile identity persisted in the marker is not bounded and canonical.
    #[error("owner profile identity is invalid")]
    InvalidOwnerProfileIdentity,
    /// The lock entry could not be opened without following a link.
    #[error("volume ownership lock unavailable at {path}: {reason}")]
    LockUnavailable { path: PathBuf, reason: String },
    /// Another cooperating process currently holds this volume's lock.
    #[error("volume ownership is held by another process at {path}")]
    OwnedByAnotherProcess { path: PathBuf },
    /// The retained lock path no longer names the acquired lock file.
    #[error("volume ownership lock was replaced at {path}")]
    LockReplaced { path: PathBuf },
    /// The fixed lock entry is not a regular file.
    #[error("volume ownership lock is not a regular file at {path}")]
    LockNotRegular { path: PathBuf },
    /// The fixed lock entry has multiple hard links and cannot bind one namespace name.
    #[error("volume ownership lock is hard-linked at {path}")]
    LockHardLinked { path: PathBuf },
    /// The fixed lock entry is not owned by the effective operating-system user.
    #[error("volume ownership lock is not owned by the effective user at {path}")]
    LockNotOwned { path: PathBuf },
    /// The marker cannot be trusted for reconciliation.
    #[error("volume ownership marker is corrupt at {path}: {reason}")]
    MarkerCorrupt { path: PathBuf, reason: String },
    /// Marker I/O failed while the descriptor remained owned by this operation.
    #[error("volume ownership marker I/O failed at {path}: {source}")]
    MarkerIo { path: PathBuf, source: io::Error },
    /// The previous owner left a dirty marker; no takeover is permitted.
    #[error(
        "volume ownership requires reconciliation for owner {owner_profile_identity} generation {generation}"
    )]
    ReconciliationRequired {
        owner_profile_identity: String,
        generation: u64,
    },
    /// A foreign dirty v1 marker is keyed only by a reusable device number, so its volume
    /// authority is ambiguous even though the current descriptor has a stronger UUID.
    #[cfg(target_os = "macos")]
    #[error(
        "ambiguous legacy dirty volume marker at {marker_path} for device {device} UUID {volume_uuid}; owner {owner_profile_identity} generation {generation}; preserve and reconcile this marker before retrying"
    )]
    AmbiguousLegacyDirtyMarker {
        device: u64,
        volume_uuid: String,
        marker_path: PathBuf,
        owner_profile_identity: String,
        generation: u64,
    },
    /// The marker changed while this capability was retained.
    #[error("volume ownership marker changed at {path}")]
    MarkerChanged { path: PathBuf },
    /// A clean handoff was attempted; this owner can no longer perform capacity work.
    #[error("volume ownership handoff was fenced at {path}")]
    HandoffFenced { path: PathBuf },
    /// Marker generations cannot wrap.
    #[error("volume ownership marker generation overflowed")]
    GenerationOverflow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MarkerState {
    Dirty,
    Clean,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct OwnershipMarker {
    state: MarkerState,
    owner_profile_identity: String,
    generation: u64,
}

/// Strong macOS identity retained separately from the still device-keyed v1 marker namespace.
#[cfg(target_os = "macos")]
#[derive(Debug)]
struct MacVolumeBinding {
    descriptor: File,
    device: u64,
    uuid: [u8; 16],
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MacVolumeObservation {
    device: u64,
    uuid: [u8; 16],
}

#[cfg(target_os = "macos")]
impl MacVolumeBinding {
    fn acquire(descriptor: &File, volume: &VolumeIdentity) -> Result<Self, VolumeOwnershipError> {
        let observation = observe_mac_volume(descriptor)?;
        if observation.device != volume.device {
            return Err(VolumeOwnershipError::VolumeIdentityUnavailable);
        }
        Ok(Self {
            descriptor: descriptor
                .try_clone()
                .map_err(|_| VolumeOwnershipError::VolumeIdentityUnavailable)?,
            device: observation.device,
            uuid: observation.uuid,
        })
    }

    fn try_clone(&self) -> Result<Self, VolumeOwnershipError> {
        Ok(Self {
            descriptor: self
                .descriptor
                .try_clone()
                .map_err(|_| VolumeOwnershipError::VolumeIdentityUnavailable)?,
            device: self.device,
            uuid: self.uuid,
        })
    }

    fn validate(&self, expected: &VolumeIdentity) -> Result<(), VolumeOwnershipError> {
        self.validate_with(expected, |descriptor| observe_mac_volume(descriptor))
    }

    fn validate_with<Observe>(
        &self,
        expected: &VolumeIdentity,
        observe: Observe,
    ) -> Result<(), VolumeOwnershipError>
    where
        Observe: FnOnce(&File) -> Result<MacVolumeObservation, VolumeOwnershipError>,
    {
        let current = observe(&self.descriptor)?;
        if expected.device != self.device
            || current.device != self.device
            || current.uuid != self.uuid
        {
            return Err(VolumeOwnershipError::VolumeIdentityUnavailable);
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
type VolumeBinding = Option<MacVolumeBinding>;

#[cfg(not(target_os = "macos"))]
type VolumeBinding = ();

#[cfg(target_os = "macos")]
fn observe_mac_volume(descriptor: &File) -> Result<MacVolumeObservation, VolumeOwnershipError> {
    use std::os::fd::AsRawFd;

    let mut attributes = libc::attrlist {
        bitmapcount: 5,
        reserved: 0,
        commonattr: 0,
        volattr: libc::ATTR_VOL_INFO | libc::ATTR_VOL_UUID,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    let mut bytes = [0_u8; 20];
    // SAFETY: both pointers refer to fully initialized, correctly sized stack values that remain
    // live for the call. The descriptor is borrowed and fgetattrlist does not take ownership.
    let result = unsafe {
        libc::fgetattrlist(
            descriptor.as_raw_fd(),
            (&mut attributes as *mut libc::attrlist).cast(),
            bytes.as_mut_ptr().cast(),
            bytes.len(),
            0,
        )
    };
    if result != 0 {
        return Err(VolumeOwnershipError::VolumeIdentityUnavailable);
    }
    let uuid = parse_mac_volume_uuid(&bytes)?;
    use std::os::unix::fs::MetadataExt;
    let device = descriptor
        .metadata()
        .map_err(|_| VolumeOwnershipError::VolumeIdentityUnavailable)?
        .dev() as u64;
    if device == 0 {
        return Err(VolumeOwnershipError::VolumeIdentityUnavailable);
    }
    Ok(MacVolumeObservation { device, uuid })
}

#[cfg(target_os = "macos")]
fn format_uuid(uuid: &[u8; 16]) -> String {
    uuid.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(target_os = "macos")]
fn parse_mac_volume_uuid(bytes: &[u8]) -> Result<[u8; 16], VolumeOwnershipError> {
    if bytes.len() != 20 || u32::from_ne_bytes(bytes[..4].try_into().unwrap()) != 20 {
        return Err(VolumeOwnershipError::VolumeIdentityUnavailable);
    }
    let uuid: [u8; 16] = bytes[4..]
        .try_into()
        .map_err(|_| VolumeOwnershipError::VolumeIdentityUnavailable)?;
    if uuid.iter().all(|byte| *byte == 0) {
        return Err(VolumeOwnershipError::VolumeIdentityUnavailable);
    }
    Ok(uuid)
}

#[derive(Debug)]
struct RegistryRoot {
    path: PathBuf,
    identity: String,
    capability: Dir,
}

impl RegistryRoot {
    fn validate(&self) -> Result<(), VolumeOwnershipError> {
        let current = open_no_follow_directory(&self.path).map_err(|source| {
            VolumeOwnershipError::RootUnavailable {
                path: self.path.clone(),
                reason: source.to_string(),
            }
        })?;
        let identity =
            identity_from_dir(&current).ok_or_else(|| VolumeOwnershipError::RootUnavailable {
                path: self.path.clone(),
                reason: String::from("registry root identity is unavailable"),
            })?;
        if identity != self.identity {
            return Err(VolumeOwnershipError::RootReplaced {
                path: self.path.clone(),
            });
        }
        Ok(())
    }
}

/// A retained per-user lock for one descriptor-derived volume identity.
///
/// The `Rc` marker intentionally makes this capability non-`Send` and non-`Sync`; a future
/// owner must retain it on the serialized journal-owner thread. Dropping it releases only the
/// operating-system lock; an orderly owner shutdown must use the private clean-handoff method
/// before dropping the capability.
#[derive(Debug)]
pub(crate) struct VolumeOwnershipCapability {
    // Close the affected locked file before its retained ancestor namespace.
    lock_file: File,
    registry: RegistryRoot,
    volume: VolumeIdentity,
    lock_path: PathBuf,
    lock_identity: String,
    marker: OwnershipMarker,
    /// Production macOS ownership retains and revalidates the original volume descriptor and
    /// UUID. Fixture providers and non-macOS builds keep the established device-only behavior.
    #[cfg(target_os = "macos")]
    mac_volume: Option<MacVolumeBinding>,
    /// Set before attempting a clean write.  A failed or uncertain write must never let this
    /// runtime continue using the old generation as if the handoff had not started.
    handoff_fenced: Rc<Cell<bool>>,
    read_fenced: Arc<AtomicBool>,
    _owner_thread: PhantomData<Rc<()>>,
}

impl Drop for VolumeOwnershipCapability {
    fn drop(&mut self) {
        // Every original context alias shares the actual flock open-file description.
        // Fence readonly observers before the LAST original alias releases ownership.
        if Rc::strong_count(&self.handoff_fenced) == 1 {
            self.read_fenced.store(true, Ordering::Release);
        }
    }
}

/// Independent readonly registry/marker and native-volume observation. No flock descriptor,
/// profile authority, Rc fence or ownership capability is transferred to this witness.
#[allow(dead_code)]
pub(super) struct ReadonlyVolumeOwner {
    registry: RegistryRoot,
    volume: VolumeIdentity,
    lock_path: PathBuf,
    lock_identity: String,
    marker: OwnershipMarker,
    #[cfg(target_os = "macos")]
    mac_volume: MacVolumeBinding,
    fenced: Arc<AtomicBool>,
}
#[allow(dead_code)]
impl ReadonlyVolumeOwner {
    pub(super) fn same_original(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.fenced, &other.fenced)
    }

    pub(super) fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.registry.path.capacity()
            + self.registry.identity.capacity()
            + self.lock_path.capacity()
            + self.lock_identity.capacity()
            + self.marker.owner_profile_identity.capacity()
    }
    pub(super) fn generation(&self) -> u64 {
        self.marker.generation
    }
    pub(super) fn volume(&self) -> &VolumeIdentity {
        &self.volume
    }
    pub(super) fn validate(&self) -> Result<(), VolumeOwnershipError> {
        if self.fenced.load(Ordering::Acquire) {
            return Err(VolumeOwnershipError::HandoffFenced {
                path: self.lock_path.clone(),
            });
        }
        #[cfg(target_os = "macos")]
        self.mac_volume.validate(&self.volume)?;
        self.registry.validate()?;
        let name = self
            .lock_path
            .file_name()
            .ok_or(VolumeOwnershipError::InvalidVolumeIdentity)?;
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        #[cfg(unix)]
        {
            use cap_primitives::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK);
        }
        let lock = self
            .registry
            .capability
            .open_with(Path::new(name), &options)
            .map_err(|source| VolumeOwnershipError::MarkerIo {
                path: self.lock_path.clone(),
                source,
            })?
            .into_std();
        validate_lock_file(&lock, &self.lock_path)?;
        if identity_from_file(&lock).as_deref() != Some(self.lock_identity.as_str()) {
            return Err(VolumeOwnershipError::LockReplaced {
                path: self.lock_path.clone(),
            });
        }
        if read_marker(lock, &self.lock_path)?.as_ref() != Some(&self.marker) {
            return Err(VolumeOwnershipError::MarkerChanged {
                path: self.lock_path.clone(),
            });
        }
        self.registry.validate()?;
        if self.fenced.load(Ordering::Acquire) {
            return Err(VolumeOwnershipError::HandoffFenced {
                path: self.lock_path.clone(),
            });
        }
        Ok(())
    }
}

impl VolumeOwnershipCapability {
    pub(super) fn legacy_metadata_bytes(&self) -> Option<usize> {
        let paths = [&self.registry.path, &self.lock_path];
        if paths
            .iter()
            .any(|p| p.as_os_str().as_encoded_bytes().len() > 4096 || p.capacity() > 8192)
            || [
                &self.registry.identity,
                &self.lock_identity,
                &self.marker.owner_profile_identity,
            ]
            .iter()
            .any(|s| s.capacity() > 256)
        {
            return None;
        }
        Some(
            std::mem::size_of::<Self>()
                + 6 * std::mem::size_of::<usize>()
                + paths.iter().map(|p| p.capacity()).sum::<usize>()
                + self.registry.identity.capacity()
                + self.lock_identity.capacity()
                + self.marker.owner_profile_identity.capacity(),
        )
    }
    /// Return the live volume identity bound to this capability.
    #[allow(dead_code)]
    pub(super) fn readonly_observation(&self) -> Result<ReadonlyVolumeOwner, VolumeOwnershipError> {
        #[cfg(not(target_os = "macos"))]
        {
            return Err(VolumeOwnershipError::Unsupported {
                path: self.lock_path.clone(),
                reason: "qualified native readonly volume witness unavailable".into(),
            });
        }
        #[cfg(target_os = "macos")]
        {
            self.revalidate()?;
            let bounded = |p: &Path| {
                p.as_os_str().as_encoded_bytes().len() <= 4096 && p.components().count() <= 64
            };
            if !bounded(&self.registry.path)
                || !bounded(&self.lock_path)
                || self.registry.identity.len() > 256
                || self.lock_identity.len() > 256
                || self.marker.owner_profile_identity.len() > 256
            {
                return Err(VolumeOwnershipError::InvalidVolumeIdentity);
            }
            let binding = self
                .mac_volume
                .as_ref()
                .ok_or(VolumeOwnershipError::VolumeIdentityUnavailable)?;
            // Do not duplicate a mutation-capable native observation descriptor.
            use std::os::fd::AsRawFd;
            let flags = unsafe { libc::fcntl(binding.descriptor.as_raw_fd(), libc::F_GETFL) };
            if flags < 0 || flags & libc::O_ACCMODE != libc::O_RDONLY {
                return Err(VolumeOwnershipError::VolumeIdentityUnavailable);
            }
            let observer = ReadonlyVolumeOwner {
                registry: RegistryRoot {
                    path: self.registry.path.to_path_buf(),
                    identity: self.registry.identity.to_string(),
                    capability: self.registry.capability.try_clone().map_err(|source| {
                        VolumeOwnershipError::MarkerIo {
                            path: self.registry.path.clone(),
                            source,
                        }
                    })?,
                },
                volume: self.volume.clone(),
                lock_path: self.lock_path.to_path_buf(),
                lock_identity: self.lock_identity.to_string(),
                marker: self.marker.clone(),
                mac_volume: binding.try_clone()?,
                fenced: Arc::clone(&self.read_fenced),
            };
            observer.validate()?;
            self.revalidate()?;
            Ok(observer)
        }
    }

    pub(super) fn volume(&self) -> &VolumeIdentity {
        &self.volume
    }

    pub(super) fn owner_profile_identity(&self) -> &str {
        &self.marker.owner_profile_identity
    }

    pub(super) fn owner_generation(&self) -> u64 {
        self.marker.generation
    }

    /// Return the only readiness level this bounded capability can provide.
    pub(super) fn readiness(&self) -> VolumeOwnershipReadiness {
        VolumeOwnershipReadiness::RecoveryOnly
    }

    /// Duplicate the retained lock descriptors for a short-lived recovery context.
    ///
    /// The duplicate shares the already-acquired OS lock and handoff fence; it never performs a
    /// second path-based acquisition. The owner runtime keeps the original capability until the
    /// journal closes.
    pub(super) fn try_clone_for_context(&self) -> Result<Self, VolumeOwnershipError> {
        let registry_capability = self.registry.capability.try_clone().map_err(|source| {
            VolumeOwnershipError::MarkerIo {
                path: self.registry.path.clone(),
                source,
            }
        })?;
        let lock_file =
            self.lock_file
                .try_clone()
                .map_err(|source| VolumeOwnershipError::MarkerIo {
                    path: self.lock_path.clone(),
                    source,
                })?;
        #[cfg(target_os = "macos")]
        let mac_volume = self
            .mac_volume
            .as_ref()
            .map(MacVolumeBinding::try_clone)
            .transpose()?;
        Ok(Self {
            registry: RegistryRoot {
                path: self.registry.path.clone(),
                identity: self.registry.identity.clone(),
                capability: registry_capability,
            },
            volume: self.volume.clone(),
            lock_path: self.lock_path.clone(),
            lock_identity: self.lock_identity.clone(),
            lock_file,
            marker: self.marker.clone(),
            #[cfg(target_os = "macos")]
            mac_volume,
            handoff_fenced: Rc::clone(&self.handoff_fenced),
            read_fenced: Arc::clone(&self.read_fenced),
            _owner_thread: PhantomData,
        })
    }

    /// Revalidate the retained registry root, lock entry, and dirty marker.
    pub(super) fn revalidate(&self) -> Result<(), VolumeOwnershipError> {
        if self.handoff_fenced.get() {
            return Err(VolumeOwnershipError::HandoffFenced {
                path: self.lock_path.clone(),
            });
        }
        #[cfg(target_os = "macos")]
        if let Some(binding) = &self.mac_volume {
            binding.validate(&self.volume)?;
        }
        self.registry.validate()?;
        let lock = open_existing_lock(&self.registry.capability, &self.lock_path)?;
        let identity =
            identity_from_file(&lock).ok_or_else(|| VolumeOwnershipError::LockUnavailable {
                path: self.lock_path.clone(),
                reason: String::from("lock identity is unavailable"),
            })?;
        if identity != self.lock_identity {
            return Err(VolumeOwnershipError::LockReplaced {
                path: self.lock_path.clone(),
            });
        }
        let marker = read_marker(lock, &self.lock_path)?;
        if marker.as_ref() != Some(&self.marker) {
            return Err(VolumeOwnershipError::MarkerChanged {
                path: self.lock_path.clone(),
            });
        }
        Ok(())
    }

    /// Durably mark this owner clean as the final step of an orderly owner-thread handoff.
    ///
    /// The capability is fenced before any bytes are changed.  A file or directory sync error
    /// therefore leaves the runtime unable to continue capacity work even when the bytes may have
    /// reached storage; callers receive no success receipt and must retain conservative evidence.
    pub(super) fn mark_clean_after_handoff(&mut self) -> Result<(), VolumeOwnershipError> {
        self.mark_clean_after_handoff_with_sync(sync_directory)
    }

    fn mark_clean_after_handoff_with_sync<Sync>(
        &mut self,
        sync: Sync,
    ) -> Result<(), VolumeOwnershipError>
    where
        Sync: Fn(&Dir, &Path) -> Result<(), VolumeOwnershipError>,
    {
        self.mark_clean_after_handoff_with_ops(write_marker, sync)
    }

    fn mark_clean_after_handoff_with_ops<WriteMarker, Sync>(
        &mut self,
        write: WriteMarker,
        sync: Sync,
    ) -> Result<(), VolumeOwnershipError>
    where
        WriteMarker: Fn(&mut File, &Path, &OwnershipMarker) -> Result<(), VolumeOwnershipError>,
        Sync: Fn(&Dir, &Path) -> Result<(), VolumeOwnershipError>,
    {
        if self.handoff_fenced.get() {
            return Err(VolumeOwnershipError::HandoffFenced {
                path: self.lock_path.clone(),
            });
        }
        self.revalidate()?;
        self.read_fenced.store(true, Ordering::Release);
        self.handoff_fenced.set(true);
        let mut clean_marker = self.marker.clone();
        clean_marker.state = MarkerState::Clean;
        let result = (|| {
            let mut lock =
                self.lock_file
                    .try_clone()
                    .map_err(|source| VolumeOwnershipError::MarkerIo {
                        path: self.lock_path.clone(),
                        source,
                    })?;
            write(&mut lock, &self.lock_path, &clean_marker)?;
            sync(&self.registry.capability, &self.registry.path)?;
            Ok::<(), VolumeOwnershipError>(())
        })();
        if result.is_ok() {
            self.marker = clean_marker;
        }
        result
    }

    #[cfg(test)]
    fn test_release_clean(&mut self) -> Result<(), VolumeOwnershipError> {
        self.mark_clean_after_handoff()
    }

    #[cfg(test)]
    fn test_mark_clean_with_sync_failure<Sync>(
        &mut self,
        sync: Sync,
    ) -> Result<(), VolumeOwnershipError>
    where
        Sync: Fn(&Dir, &Path) -> Result<(), VolumeOwnershipError>,
    {
        self.mark_clean_after_handoff_with_sync(sync)
    }

    #[cfg(test)]
    fn test_mark_clean_with_write_failure(&mut self) -> Result<(), VolumeOwnershipError> {
        self.mark_clean_after_handoff_with_ops(
            |_, path, _| {
                Err(VolumeOwnershipError::MarkerIo {
                    path: path.to_path_buf(),
                    source: io::Error::other("injected pre-write failure"),
                })
            },
            sync_directory,
        )
    }

    #[cfg(test)]
    fn lock_path_for_test(&self) -> &Path {
        &self.lock_path
    }

    #[cfg(test)]
    pub(crate) fn marker_is_clean_for_test(&self) -> bool {
        self.marker.state == MarkerState::Clean
    }
}

/// Acquire the shared per-user volume lock from a live descriptor and profile guard.
///
/// The production root is derived from the operating-system account, not profile or process
/// configuration. The returned capability is recovery-only and cannot authorize ordinary work.
pub(super) fn acquire_for_descriptor(
    descriptor: &File,
    profile: &WritableProfileGuard,
) -> Result<VolumeOwnershipCapability, VolumeOwnershipError> {
    profile
        .validate_current()
        .map_err(|error| root_error(PathBuf::from("profile-owner.lock"), error))?;
    let facts = descriptor_capacity_facts(descriptor)
        .map_err(|_| VolumeOwnershipError::VolumeIdentityUnavailable)?;
    #[cfg(target_os = "macos")]
    let mac_volume = Some(MacVolumeBinding::acquire(descriptor, &facts.identity)?);
    #[cfg(not(target_os = "macos"))]
    let mac_volume = ();
    let registry = production_registry_root()?;
    acquire_inner_with_binding(
        registry,
        facts.identity,
        profile.profile_root_identity().to_owned(),
        false,
        mac_volume,
    )
}

/// Acquire a volume owner for restart after a confirmed clean marker.  The acquisition writes a
/// new dirty generation while the lock is held; the caller must complete the private durable-zero
/// and physical-proof checks before it can rebind the reserve generation or mark the owner clean.
pub(super) fn acquire_for_descriptor_after_clean_handoff(
    descriptor: &File,
    profile: &WritableProfileGuard,
) -> Result<VolumeOwnershipCapability, VolumeOwnershipError> {
    profile
        .validate_current()
        .map_err(|error| root_error(PathBuf::from("profile-owner.lock"), error))?;
    let facts = descriptor_capacity_facts(descriptor)
        .map_err(|_| VolumeOwnershipError::VolumeIdentityUnavailable)?;
    #[cfg(target_os = "macos")]
    let mac_volume = Some(MacVolumeBinding::acquire(descriptor, &facts.identity)?);
    #[cfg(not(target_os = "macos"))]
    let mac_volume = ();
    let registry = production_registry_root()?;
    acquire_inner_with_binding(
        registry,
        facts.identity,
        profile.profile_root_identity().to_owned(),
        true,
        mac_volume,
    )
}

fn production_registry_root() -> Result<RegistryRoot, VolumeOwnershipError> {
    let (app_path, app_capability) =
        open_canonical_global_app_root().map_err(|error| match error {
            ProfileOwnershipError::Unsupported { path } => VolumeOwnershipError::Unsupported {
                path,
                reason: String::from("no qualified identity-safe per-user application root"),
            },
            other => root_error(PathBuf::from(".yield"), other),
        })?;
    open_registry_child(app_path, app_capability)
}

fn open_registry_child(
    app_path: PathBuf,
    app_capability: Dir,
) -> Result<RegistryRoot, VolumeOwnershipError> {
    open_registry_child_with_sync(app_path, app_capability, sync_directory)
}

fn open_registry_child_with_sync<Sync>(
    app_path: PathBuf,
    app_capability: Dir,
    sync: Sync,
) -> Result<RegistryRoot, VolumeOwnershipError>
where
    Sync: Fn(&Dir, &Path) -> Result<(), VolumeOwnershipError>,
{
    let registry_path = app_path.join(REGISTRY_DIRECTORY_NAME);
    let registry_capability = match app_capability.open_dir_nofollow(REGISTRY_DIRECTORY_NAME) {
        Ok(capability) => capability,
        Err(error) if error.kind() == io::ErrorKind::NotFound => app_capability
            .create_dir(REGISTRY_DIRECTORY_NAME)
            .or_else(|create_error| {
                if create_error.kind() == io::ErrorKind::AlreadyExists {
                    Ok(())
                } else {
                    Err(create_error)
                }
            })
            .and_then(|_| app_capability.open_dir_nofollow(REGISTRY_DIRECTORY_NAME))
            .map_err(|source| VolumeOwnershipError::RootUnavailable {
                path: registry_path.clone(),
                reason: source.to_string(),
            })?,
        Err(error) => {
            return Err(VolumeOwnershipError::RootUnavailable {
                path: registry_path,
                reason: error.to_string(),
            });
        }
    };
    sync(&app_capability, &app_path)?;
    let identity = identity_from_dir(&registry_capability).ok_or_else(|| {
        VolumeOwnershipError::RootUnavailable {
            path: registry_path.clone(),
            reason: String::from("registry root identity is unavailable"),
        }
    })?;
    Ok(RegistryRoot {
        path: registry_path,
        identity,
        capability: registry_capability,
    })
}

fn lock_file_name(volume: &VolumeIdentity) -> Result<String, VolumeOwnershipError> {
    // OPT-1934: retain the shared v1 device namespace. A UUID primary plus
    // aliases cannot exclude shipped v1 binaries at a previously unseen device.
    // UUID binding is descriptor validation only until an enforceable launch
    // exclusion boundary is qualified (see GPUI_VOLUME_UPGRADE_EXCLUSION.md).
    if volume.device == 0 {
        return Err(VolumeOwnershipError::InvalidVolumeIdentity);
    }
    Ok(format!(
        "{LOCK_FILE_PREFIX}{}{LOCK_FILE_SUFFIX}",
        volume.device
    ))
}

#[cfg(any(test, feature = "test-support"))]
fn acquire_inner(
    registry: RegistryRoot,
    volume: VolumeIdentity,
    owner_profile_identity: String,
    allow_clean_marker: bool,
) -> Result<VolumeOwnershipCapability, VolumeOwnershipError> {
    #[cfg(target_os = "macos")]
    let binding = None;
    #[cfg(not(target_os = "macos"))]
    let binding = ();
    acquire_inner_with_binding(
        registry,
        volume,
        owner_profile_identity,
        allow_clean_marker,
        binding,
    )
}

fn acquire_inner_with_binding(
    registry: RegistryRoot,
    volume: VolumeIdentity,
    owner_profile_identity: String,
    allow_clean_marker: bool,
    binding: VolumeBinding,
) -> Result<VolumeOwnershipCapability, VolumeOwnershipError> {
    acquire_inner_with_binding_and_sync(
        registry,
        volume,
        owner_profile_identity,
        allow_clean_marker,
        binding,
        sync_directory,
    )
}

#[cfg(any(test, feature = "test-support"))]
fn acquire_inner_with_sync<Sync>(
    registry: RegistryRoot,
    volume: VolumeIdentity,
    owner_profile_identity: String,
    allow_clean_marker: bool,
    sync: Sync,
) -> Result<VolumeOwnershipCapability, VolumeOwnershipError>
where
    Sync: Fn(&Dir, &Path) -> Result<(), VolumeOwnershipError>,
{
    #[cfg(target_os = "macos")]
    let binding = None;
    #[cfg(not(target_os = "macos"))]
    let binding = ();
    acquire_inner_with_binding_and_sync(
        registry,
        volume,
        owner_profile_identity,
        allow_clean_marker,
        binding,
        sync,
    )
}

fn acquire_inner_with_binding_and_sync<Sync>(
    registry: RegistryRoot,
    volume: VolumeIdentity,
    owner_profile_identity: String,
    allow_clean_marker: bool,
    binding: VolumeBinding,
    sync: Sync,
) -> Result<VolumeOwnershipCapability, VolumeOwnershipError>
where
    Sync: Fn(&Dir, &Path) -> Result<(), VolumeOwnershipError>,
{
    #[cfg(not(target_os = "macos"))]
    let _ = binding;
    validate_owner_profile_identity(&owner_profile_identity)?;
    registry.validate()?;
    let name = lock_file_name(&volume)?;
    let lock_path = registry.path.join(&name);
    let lock_file = open_lock(&registry.capability, &name, &lock_path)?;
    sync(&registry.capability, &registry.path)?;
    let lock_identity =
        identity_from_file(&lock_file).ok_or_else(|| VolumeOwnershipError::LockUnavailable {
            path: lock_path.clone(),
            reason: String::from("lock identity is unavailable"),
        })?;
    try_exclusive_lock(&lock_file, &lock_path)?;

    #[cfg(target_os = "macos")]
    if let Some(binding) = &binding {
        binding.validate(&volume)?;
    }

    let previous = read_marker(
        lock_file
            .try_clone()
            .map_err(|source| VolumeOwnershipError::MarkerIo {
                path: lock_path.clone(),
                source,
            })?,
        &lock_path,
    )?;
    #[cfg(target_os = "macos")]
    if let (Some(binding), Some(marker)) = (&binding, &previous) {
        if marker.state == MarkerState::Dirty
            && marker.owner_profile_identity != owner_profile_identity
        {
            return Err(VolumeOwnershipError::AmbiguousLegacyDirtyMarker {
                device: volume.device,
                volume_uuid: format_uuid(&binding.uuid),
                marker_path: lock_path,
                owner_profile_identity: marker.owner_profile_identity.clone(),
                generation: marker.generation,
            });
        }
    }
    let (marker, persist_marker) =
        next_dirty_marker(previous, &owner_profile_identity, allow_clean_marker)?;
    if persist_marker {
        let mut writable_lock =
            lock_file
                .try_clone()
                .map_err(|source| VolumeOwnershipError::MarkerIo {
                    path: lock_path.clone(),
                    source,
                })?;
        write_marker(&mut writable_lock, &lock_path, &marker)?;
    }

    let capability = VolumeOwnershipCapability {
        registry,
        volume,
        lock_path,
        lock_identity,
        lock_file,
        marker,
        #[cfg(target_os = "macos")]
        mac_volume: binding,
        handoff_fenced: Rc::new(Cell::new(false)),
        read_fenced: Arc::new(AtomicBool::new(false)),
        _owner_thread: PhantomData,
    };
    capability.revalidate()?;
    Ok(capability)
}

fn next_dirty_marker(
    previous: Option<OwnershipMarker>,
    owner_profile_identity: &str,
    allow_clean_marker: bool,
) -> Result<(OwnershipMarker, bool), VolumeOwnershipError> {
    let generation = match previous {
        None => 1,
        Some(marker) => match marker.state {
            MarkerState::Dirty => {
                if marker.owner_profile_identity == owner_profile_identity {
                    return Ok((marker, false));
                }
                return Err(VolumeOwnershipError::ReconciliationRequired {
                    owner_profile_identity: marker.owner_profile_identity,
                    generation: marker.generation,
                });
            }
            MarkerState::Clean if !allow_clean_marker => {
                return Err(VolumeOwnershipError::ReconciliationRequired {
                    owner_profile_identity: marker.owner_profile_identity,
                    generation: marker.generation,
                });
            }
            MarkerState::Clean => marker
                .generation
                .checked_add(1)
                .ok_or(VolumeOwnershipError::GenerationOverflow)?,
        },
    };
    Ok((
        OwnershipMarker {
            state: MarkerState::Dirty,
            owner_profile_identity: owner_profile_identity.to_owned(),
            generation,
        },
        true,
    ))
}

fn validate_owner_profile_identity(value: &str) -> Result<(), VolumeOwnershipError> {
    if value.is_empty()
        || value.len() > MAX_IDENTITY_BYTES
        || value
            .bytes()
            .any(|byte| matches!(byte, b'\n' | b'\r' | b'='))
    {
        return Err(VolumeOwnershipError::InvalidOwnerProfileIdentity);
    }
    Ok(())
}

fn read_marker(
    mut file: File,
    path: &Path,
) -> Result<Option<OwnershipMarker>, VolumeOwnershipError> {
    file.seek(SeekFrom::Start(0))
        .and_then(|_| {
            let mut bytes = Vec::new();
            file.take((MAX_MARKER_BYTES + 1) as u64)
                .read_to_end(&mut bytes)?;
            Ok(bytes)
        })
        .map_err(|source| VolumeOwnershipError::MarkerIo {
            path: path.to_path_buf(),
            source,
        })
        .and_then(|bytes| {
            if bytes.is_empty() {
                return Ok(None);
            }
            if bytes.len() > MAX_MARKER_BYTES {
                return Err(VolumeOwnershipError::MarkerCorrupt {
                    path: path.to_path_buf(),
                    reason: String::from("marker exceeds bounded size"),
                });
            }
            parse_marker(&bytes)
                .map(Some)
                .map_err(|reason| VolumeOwnershipError::MarkerCorrupt {
                    path: path.to_path_buf(),
                    reason,
                })
        })
}

fn parse_marker(bytes: &[u8]) -> Result<OwnershipMarker, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| String::from("marker is not UTF-8"))?;
    let mut version = None;
    let mut state = None;
    let mut owner_profile_identity = None;
    let mut generation = None;
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| String::from("marker line has no key/value separator"))?;
        match key {
            "version" if version.is_none() => version = Some(value),
            "state" if state.is_none() => state = Some(value),
            "owner_profile_identity" if owner_profile_identity.is_none() => {
                owner_profile_identity = Some(value)
            }
            "generation" if generation.is_none() => generation = Some(value),
            _ => {
                return Err(format!(
                    "marker contains an unknown or duplicate field {key:?}"
                ));
            }
        }
    }
    if version != Some(MARKER_VERSION) {
        return Err(String::from("marker version is unsupported"));
    }
    let state = match state {
        Some("dirty") => MarkerState::Dirty,
        Some("clean") => MarkerState::Clean,
        _ => return Err(String::from("marker state is unsupported")),
    };
    let owner_profile_identity = owner_profile_identity
        .ok_or_else(|| String::from("marker owner profile identity is missing"))?
        .to_owned();
    validate_owner_profile_identity(&owner_profile_identity)
        .map_err(|_| String::from("marker owner profile identity is invalid"))?;
    let generation = generation.ok_or_else(|| String::from("marker generation is missing"))?;
    let generation = generation
        .parse::<u64>()
        .map_err(|_| String::from("marker generation is not canonical u64 text"))?;
    if generation == 0 {
        return Err(String::from("marker generation is zero"));
    }
    Ok(OwnershipMarker {
        state,
        owner_profile_identity,
        generation,
    })
}

fn write_marker(
    file: &mut File,
    path: &Path,
    marker: &OwnershipMarker,
) -> Result<(), VolumeOwnershipError> {
    let state = match marker.state {
        MarkerState::Dirty => "dirty",
        MarkerState::Clean => "clean",
    };
    let contents = format!(
        "version={MARKER_VERSION}\nstate={state}\nowner_profile_identity={}\ngeneration={}\n",
        marker.owner_profile_identity, marker.generation
    );
    if contents.len() > MAX_MARKER_BYTES {
        return Err(VolumeOwnershipError::MarkerCorrupt {
            path: path.to_path_buf(),
            reason: String::from("marker exceeds bounded size"),
        });
    }
    file.set_len(0)
        .and_then(|_| file.seek(SeekFrom::Start(0)).map(|_| ()))
        .and_then(|_| file.write_all(contents.as_bytes()))
        .and_then(|_| file.sync_all())
        .map_err(|source| VolumeOwnershipError::MarkerIo {
            path: path.to_path_buf(),
            source,
        })
}

fn open_lock(registry: &Dir, name: &str, path: &Path) -> Result<File, VolumeOwnershipError> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .follow(FollowSymlinks::No);
    let file = registry
        .open_with(Path::new(name), &options)
        .map_err(|source| VolumeOwnershipError::LockUnavailable {
            path: path.to_path_buf(),
            reason: source.to_string(),
        })?
        .into_std();
    validate_lock_file(&file, path)?;
    Ok(file)
}

fn open_existing_lock(registry: &Dir, path: &Path) -> Result<File, VolumeOwnershipError> {
    let name = path
        .file_name()
        .ok_or_else(|| VolumeOwnershipError::LockUnavailable {
            path: path.to_path_buf(),
            reason: String::from("lock path has no file name"),
        })?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).follow(FollowSymlinks::No);
    let file = registry
        .open_with(Path::new(name), &options)
        .map_err(|source| VolumeOwnershipError::LockUnavailable {
            path: path.to_path_buf(),
            reason: source.to_string(),
        })?
        .into_std();
    validate_lock_file(&file, path)?;
    Ok(file)
}

fn validate_lock_file(file: &File, path: &Path) -> Result<(), VolumeOwnershipError> {
    let metadata = file
        .metadata()
        .map_err(|source| VolumeOwnershipError::LockUnavailable {
            path: path.to_path_buf(),
            reason: source.to_string(),
        })?;
    #[cfg(windows)]
    {
        let _ = metadata;
        windows_lock::validate(file, path)?;
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::MetadataExt;

        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(VolumeOwnershipError::LockNotRegular {
                path: path.to_path_buf(),
            });
        }
        if metadata.nlink() != 1 {
            return Err(VolumeOwnershipError::LockHardLinked {
                path: path.to_path_buf(),
            });
        }
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(VolumeOwnershipError::LockNotOwned {
                path: path.to_path_buf(),
            });
        }
    }
    Ok(())
}

fn sync_directory(directory: &Dir, path: &Path) -> Result<(), VolumeOwnershipError> {
    #[cfg(windows)]
    {
        let _ = (directory, path);
        Ok(())
    }
    #[cfg(not(windows))]
    {
        directory
            .try_clone()
            .and_then(|directory| crate::directory_sync::sync_directory(&directory.into_std_file()))
            .map_err(|source| VolumeOwnershipError::RootUnavailable {
                path: path.to_path_buf(),
                reason: format!("failed to flush ownership namespace: {source}"),
            })
    }
}

fn try_exclusive_lock(file: &File, path: &Path) -> Result<(), VolumeOwnershipError> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;

        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            return Ok(());
        }
        let source = io::Error::last_os_error();
        if source.kind() == io::ErrorKind::WouldBlock {
            return Err(VolumeOwnershipError::OwnedByAnotherProcess {
                path: path.to_path_buf(),
            });
        }
        return Err(VolumeOwnershipError::LockUnavailable {
            path: path.to_path_buf(),
            reason: source.to_string(),
        });
    }

    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::{ERROR_LOCK_VIOLATION, HANDLE, WIN32_ERROR};
        use windows::Win32::Storage::FileSystem::{
            LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
        };
        use windows::Win32::System::IO::OVERLAPPED;

        let mut overlapped = OVERLAPPED::default();
        let result = unsafe {
            LockFileEx(
                HANDLE(file.as_raw_handle()),
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                None,
                u32::MAX,
                u32::MAX,
                &mut overlapped,
            )
        };
        match result {
            Ok(()) => Ok(()),
            Err(error) if WIN32_ERROR::from_error(&error) == Some(ERROR_LOCK_VIOLATION) => {
                Err(VolumeOwnershipError::OwnedByAnotherProcess {
                    path: path.to_path_buf(),
                })
            }
            Err(error) => Err(VolumeOwnershipError::LockUnavailable {
                path: path.to_path_buf(),
                reason: error.to_string(),
            }),
        }
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        Err(VolumeOwnershipError::Unsupported {
            path: path.to_path_buf(),
            reason: String::from("no qualified nonblocking filesystem lock primitive"),
        })
    }
}

fn identity_from_dir(dir: &Dir) -> Option<String> {
    let file = dir.try_clone().ok()?.into_std_file();
    stable_filesystem_identity_from_open_file(&file)
}

fn identity_from_file(file: &File) -> Option<String> {
    stable_filesystem_identity_from_open_file(file)
}

fn root_error(path: PathBuf, error: ProfileOwnershipError) -> VolumeOwnershipError {
    VolumeOwnershipError::RootUnavailable {
        path,
        reason: error.to_string(),
    }
}

#[cfg(any(test, feature = "test-support"))]
pub(super) fn acquire_fixture_for_test(
    descriptor: &File,
    registry_path: &Path,
    volume: VolumeIdentity,
    owner_profile_identity: &str,
) -> Result<VolumeOwnershipCapability, VolumeOwnershipError> {
    acquire_fixture_with_clean_marker_for_test(
        descriptor,
        registry_path,
        volume,
        owner_profile_identity,
        true,
    )
}

#[cfg(any(test, feature = "test-support"))]
pub(super) fn acquire_for_descriptor_fixture_test(
    descriptor: &File,
    registry_path: &Path,
    profile: &WritableProfileGuard,
) -> Result<VolumeOwnershipCapability, VolumeOwnershipError> {
    let facts = descriptor_capacity_facts(descriptor)
        .map_err(|_| VolumeOwnershipError::VolumeIdentityUnavailable)?;
    acquire_fixture_for_test(
        descriptor,
        registry_path,
        facts.identity,
        profile.profile_root_identity(),
    )
}

#[cfg(any(test, feature = "test-support"))]
pub(super) fn acquire_fixture_with_clean_marker_for_test(
    descriptor: &File,
    registry_path: &Path,
    volume: VolumeIdentity,
    owner_profile_identity: &str,
    allow_clean_marker: bool,
) -> Result<VolumeOwnershipCapability, VolumeOwnershipError> {
    descriptor
        .metadata()
        .map_err(|_| VolumeOwnershipError::VolumeIdentityUnavailable)?;
    let capability = open_no_follow_directory(registry_path).map_err(|source| {
        VolumeOwnershipError::RootUnavailable {
            path: registry_path.to_path_buf(),
            reason: source.to_string(),
        }
    })?;
    let identity =
        identity_from_dir(&capability).ok_or_else(|| VolumeOwnershipError::RootUnavailable {
            path: registry_path.to_path_buf(),
            reason: String::from("registry root identity is unavailable"),
        })?;
    acquire_inner(
        RegistryRoot {
            path: registry_path.to_path_buf(),
            identity,
            capability,
        },
        volume,
        owner_profile_identity.to_owned(),
        allow_clean_marker,
    )
}

#[cfg(all(test, target_os = "macos"))]
pub(super) fn acquire_native_reader_fixture_test(
    descriptor: &File,
    registry_path: &Path,
    profile: &WritableProfileGuard,
) -> Result<VolumeOwnershipCapability, VolumeOwnershipError> {
    profile
        .validate_current()
        .map_err(|error| root_error(registry_path.to_path_buf(), error))?;
    let facts = descriptor_capacity_facts(descriptor)
        .map_err(|_| VolumeOwnershipError::VolumeIdentityUnavailable)?;
    let binding = MacVolumeBinding::acquire(descriptor, &facts.identity)?;
    let capability = open_no_follow_directory(registry_path).map_err(|error| {
        VolumeOwnershipError::RootUnavailable {
            path: registry_path.to_path_buf(),
            reason: error.to_string(),
        }
    })?;
    let identity =
        identity_from_dir(&capability).ok_or(VolumeOwnershipError::VolumeIdentityUnavailable)?;
    acquire_inner_with_binding(
        RegistryRoot {
            path: registry_path.to_path_buf(),
            identity,
            capability,
        },
        facts.identity,
        profile.profile_root_identity().to_owned(),
        false,
        Some(binding),
    )
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    mod process_isolation {
        include!("capacity_volume_owner/tests/process_isolation.rs");
    }

    use super::*;
    use std::fs::{self, OpenOptions as StdOpenOptions};
    #[cfg(unix)]
    use std::{
        io::{self, BufRead, BufReader, Write},
        process::{Command, Stdio},
    };

    struct Fixture {
        directory: tempfile::TempDir,
        registry: PathBuf,
        descriptor: File,
    }

    fn fixture() -> Fixture {
        let temp_root = fs::canonicalize(std::env::temp_dir()).expect("canonical temp root");
        let directory = tempfile::tempdir_in(temp_root).expect("volume owner fixture");
        let registry = directory.path().join(REGISTRY_DIRECTORY_NAME);
        fs::create_dir(&registry).expect("volume owner registry");
        let descriptor_path = directory.path().join("descriptor");
        fs::write(&descriptor_path, b"small descriptor fixture").expect("descriptor fixture");
        let descriptor = StdOpenOptions::new()
            .read(true)
            .write(true)
            .open(&descriptor_path)
            .expect("open descriptor fixture");
        Fixture {
            directory,
            registry,
            descriptor,
        }
    }

    fn volume(device: u64) -> VolumeIdentity {
        VolumeIdentity { device }
    }

    #[test]
    fn native_reader_last_original_volume_owner_alias_fences_before_lock_release() {
        #[cfg(target_os = "linux")]
        if process_isolation::run_in_child(
            "native_reader_last_original_volume_owner_alias_fences_before_lock_release",
        ) {
            return;
        }
        let fixture = fixture();
        let owner = acquire_fixture_for_test(
            &fixture.descriptor,
            &fixture.registry,
            volume(77),
            "reader-profile",
        )
        .unwrap();
        let observed_fence = Arc::clone(&owner.read_fenced);
        let context = owner.try_clone_for_context().unwrap();
        assert!(!observed_fence.load(Ordering::Acquire));
        drop(owner);
        assert!(!observed_fence.load(Ordering::Acquire));
        context.revalidate().unwrap();
        assert!(
            acquire_fixture_for_test(
                &fixture.descriptor,
                &fixture.registry,
                volume(77),
                "reader-profile"
            )
            .is_err()
        );
        drop(context);
        assert!(observed_fence.load(Ordering::Acquire));
        let reacquired = acquire_fixture_for_test(
            &fixture.descriptor,
            &fixture.registry,
            volume(77),
            "reader-profile",
        )
        .unwrap();
        assert!(!reacquired.read_fenced.load(Ordering::Acquire));
        assert!(observed_fence.load(Ordering::Acquire));
    }
    #[test]
    fn old_v1_unseen_device_ignores_uuid_dirty_owner_and_active_policy_keeps_shared_key() {
        #[cfg(target_os = "linux")]
        if process_isolation::run_in_child(
            "old_v1_unseen_device_ignores_uuid_dirty_owner_and_active_policy_keeps_shared_key",
        ) {
            return;
        }
        let fixture = fixture();
        let modern = acquire_fixture_for_test(
            &fixture.descriptor,
            &fixture.registry,
            volume(41),
            "new-profile",
        )
        .unwrap();
        let known_device_path = modern.lock_path_for_test().to_owned();
        let known_dirty = fs::read(&known_device_path).unwrap();
        drop(modern); // Dirty exit; preserve its device marker.

        // Hypothetical UUID primary for A. This is fixture data, not activation
        // of a new production namespace or a claim about an actual remount.
        let uuid_primary = fixture.registry.join("volume-uuid-A.lock");
        let uuid_dirty = b"version=2\nvolume_uuid=A\nstate=dirty\nowner_profile_identity=new-profile\ngeneration=1\n";
        fs::write(&uuid_primary, uuid_dirty).unwrap();
        let unseen_path = fixture.registry.join("volume-42.lock");
        assert!(!unseen_path.exists());

        // Emulate shipped v1 lookup: only the current device file is opened.
        // It never consults a UUID primary, known-device alias or new launcher.
        let mut old = StdOpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&unseen_path)
            .unwrap();
        try_exclusive_lock(&old, &unseen_path).unwrap();
        let prior = read_marker(old.try_clone().unwrap(), &unseen_path).unwrap();
        assert!(prior.is_none());
        let (marker, persist) = next_dirty_marker(prior, "old-profile", true).unwrap();
        assert!(persist);
        write_marker(&mut old, &unseen_path, &marker).unwrap();

        // The retained policy makes the cooperating new binary contend on
        // that same v1 file, rather than admitting under the hypothetical UUID.
        assert_eq!(lock_file_name(&volume(42)).unwrap(), "volume-42.lock");
        let rejected = acquire_fixture_for_test(
            &fixture.descriptor,
            &fixture.registry,
            volume(42),
            "new-profile",
        )
        .unwrap_err();
        assert!(matches!(
            rejected,
            VolumeOwnershipError::OwnedByAnotherProcess { .. }
        ));
        assert_eq!(fs::read(&known_device_path).unwrap(), known_dirty);
        assert_eq!(fs::read(&uuid_primary).unwrap(), uuid_dirty);
        drop(old);
        let old_dirty = fs::read(&unseen_path).unwrap();
        let reacquired = acquire_fixture_for_test(
            &fixture.descriptor,
            &fixture.registry,
            volume(42),
            "new-profile",
        );
        assert!(
            matches!(
                reacquired,
                Err(VolumeOwnershipError::ReconciliationRequired { .. })
            ),
            "expected retained dirty marker after final old owner close: {reacquired:?}"
        );
        assert_eq!(fs::read(&unseen_path).unwrap(), old_dirty);
    }

    #[test]
    fn capability_uses_fixture_volume_and_is_recovery_only() {
        let fixture = fixture();
        let capability = acquire_fixture_for_test(
            &fixture.descriptor,
            &fixture.registry,
            volume(11),
            "profile-one",
        )
        .expect("fixture volume owner");
        assert_eq!(capability.volume(), &volume(11));
        assert_eq!(
            capability.readiness(),
            VolumeOwnershipReadiness::RecoveryOnly
        );
        capability.revalidate().expect("owner revalidation");
        drop(capability);
        drop(fixture);
    }

    #[test]
    fn registry_and_lock_sync_failures_preserve_entries_without_capability() {
        let fixture = fixture();
        let app_path = fixture.directory.path().join("sync-failure-app");
        fs::create_dir(&app_path).expect("sync failure app root");
        let app_capability = open_no_follow_directory(&app_path).expect("open app root");
        let registry_error =
            open_registry_child_with_sync(app_path.clone(), app_capability, |_, path| {
                Err(VolumeOwnershipError::RootUnavailable {
                    path: path.to_path_buf(),
                    reason: String::from("injected registry sync failure"),
                })
            })
            .expect_err("registry sync failure must reject capability");
        assert!(matches!(
            registry_error,
            VolumeOwnershipError::RootUnavailable { .. }
        ));
        assert!(app_path.join(REGISTRY_DIRECTORY_NAME).is_dir());

        let registry_capability =
            open_no_follow_directory(&fixture.registry).expect("open fixture registry");
        let registry_identity = identity_from_dir(&registry_capability).expect("registry identity");
        let registry = RegistryRoot {
            path: fixture.registry.clone(),
            identity: registry_identity,
            capability: registry_capability,
        };
        let lock_path = fixture.registry.join("volume-25.lock");
        let lock_error = acquire_inner_with_sync(
            registry,
            volume(25),
            String::from("profile-one"),
            false,
            |_, path| {
                Err(VolumeOwnershipError::RootUnavailable {
                    path: path.to_path_buf(),
                    reason: String::from("injected lock-directory sync failure"),
                })
            },
        )
        .expect_err("lock sync failure must reject capability");
        assert!(matches!(
            lock_error,
            VolumeOwnershipError::RootUnavailable { .. }
        ));
        assert!(lock_path.is_file());
        assert!(
            fs::read(&lock_path)
                .expect("preserved lock entry")
                .is_empty()
        );
        drop(fixture);
    }

    #[test]
    fn same_volume_profiles_contend_and_dirty_marker_requires_reconciliation() {
        #[cfg(target_os = "linux")]
        if process_isolation::run_in_child(
            "same_volume_profiles_contend_and_dirty_marker_requires_reconciliation",
        ) {
            return;
        }
        let fixture = fixture();
        let first = acquire_fixture_for_test(
            &fixture.descriptor,
            &fixture.registry,
            volume(12),
            "profile-one",
        )
        .expect("first volume owner");
        let conflict = acquire_fixture_for_test(
            &fixture.descriptor,
            &fixture.registry,
            volume(12),
            "profile-two",
        )
        .expect_err("same volume must contend");
        assert!(matches!(
            conflict,
            VolumeOwnershipError::OwnedByAnotherProcess { .. }
        ));
        drop(first);

        let stale = acquire_fixture_for_test(
            &fixture.descriptor,
            &fixture.registry,
            volume(12),
            "profile-two",
        )
        .expect_err("dirty marker must block takeover");
        assert!(matches!(
            stale,
            VolumeOwnershipError::ReconciliationRequired {
                owner_profile_identity,
                generation: 1,
            } if owner_profile_identity == "profile-one"
        ));
        drop(fixture);
    }

    #[test]
    fn same_profile_dirty_marker_reacquires_without_rewriting_generation() {
        #[cfg(target_os = "linux")]
        if process_isolation::run_in_child(
            "same_profile_dirty_marker_reacquires_without_rewriting_generation",
        ) {
            return;
        }
        let fixture = fixture();
        let first = acquire_fixture_for_test(
            &fixture.descriptor,
            &fixture.registry,
            volume(13),
            "profile-one",
        )
        .expect("first volume owner");
        let marker_path = first.lock_path_for_test().to_owned();
        let original_marker = fs::read(&marker_path).expect("read dirty marker");
        drop(first);

        let second = acquire_fixture_for_test(
            &fixture.descriptor,
            &fixture.registry,
            volume(13),
            "profile-one",
        )
        .expect("same profile may resume dirty ownership");
        assert_eq!(second.marker.generation, 1);
        assert_eq!(second.marker.owner_profile_identity, "profile-one");
        assert_eq!(
            fs::read(&marker_path).expect("read retained dirty marker"),
            original_marker
        );
        drop(second);
        drop(fixture);
    }

    #[test]
    fn independent_volume_fixtures_can_be_held_together() {
        let fixture = fixture();
        let first = acquire_fixture_for_test(
            &fixture.descriptor,
            &fixture.registry,
            volume(21),
            "profile-one",
        )
        .expect("first independent volume owner");
        let second = acquire_fixture_for_test(
            &fixture.descriptor,
            &fixture.registry,
            volume(22),
            "profile-two",
        )
        .expect("second independent volume owner");
        assert_ne!(first.volume(), second.volume());
        drop(second);
        drop(first);
        drop(fixture);
    }

    #[test]
    fn test_only_clean_release_advances_generation() {
        #[cfg(target_os = "linux")]
        if process_isolation::run_in_child("test_only_clean_release_advances_generation") {
            return;
        }
        let fixture = fixture();
        let mut first = acquire_fixture_for_test(
            &fixture.descriptor,
            &fixture.registry,
            volume(23),
            "profile-one",
        )
        .expect("first volume owner");
        first
            .test_release_clean()
            .expect("fixture clean release marker");
        drop(first);

        let second = acquire_fixture_for_test(
            &fixture.descriptor,
            &fixture.registry,
            volume(23),
            "profile-two",
        )
        .expect("fixture clean marker reacquisition");
        assert_eq!(second.marker.generation, 2);
        assert_eq!(second.marker.state, MarkerState::Dirty);
        drop(second);
        drop(fixture);
    }

    #[test]
    fn clean_handoff_fences_the_runtime_before_unlock() {
        #[cfg(target_os = "linux")]
        if process_isolation::run_in_child("clean_handoff_fences_the_runtime_before_unlock") {
            return;
        }
        let fixture = fixture();
        let mut first = acquire_fixture_for_test(
            &fixture.descriptor,
            &fixture.registry,
            volume(26),
            "profile-one",
        )
        .expect("first volume owner");
        first
            .mark_clean_after_handoff()
            .expect("clean marker handoff");
        assert!(matches!(
            first.revalidate(),
            Err(VolumeOwnershipError::HandoffFenced { .. })
        ));
        let marker = fs::read(first.lock_path_for_test()).expect("clean marker");
        assert!(String::from_utf8_lossy(&marker).contains("state=clean"));
        drop(first);
        let second = acquire_fixture_for_test(
            &fixture.descriptor,
            &fixture.registry,
            volume(26),
            "profile-one",
        )
        .expect("same profile clean restart");
        assert_eq!(second.marker.generation, 2);
        drop(second);
        drop(fixture);
    }

    #[test]
    fn uncertain_registry_sync_keeps_lock_and_never_reports_clean_success() {
        #[cfg(target_os = "linux")]
        if process_isolation::run_in_child(
            "uncertain_registry_sync_keeps_lock_and_never_reports_clean_success",
        ) {
            return;
        }
        let fixture = fixture();
        let mut first = acquire_fixture_for_test(
            &fixture.descriptor,
            &fixture.registry,
            volume(27),
            "profile-one",
        )
        .expect("first volume owner");
        let error = first
            .test_mark_clean_with_sync_failure(|_, path| {
                Err(VolumeOwnershipError::RootUnavailable {
                    path: path.to_path_buf(),
                    reason: String::from("injected post-write sync uncertainty"),
                })
            })
            .expect_err("post-write sync failure is not a clean receipt");
        assert!(matches!(
            error,
            VolumeOwnershipError::RootUnavailable { .. }
        ));
        assert!(matches!(
            first.revalidate(),
            Err(VolumeOwnershipError::HandoffFenced { .. })
        ));
        let held = acquire_fixture_for_test(
            &fixture.descriptor,
            &fixture.registry,
            volume(27),
            "profile-one",
        )
        .expect_err("uncertain handoff retains the lock while runtime lives");
        assert!(matches!(
            held,
            VolumeOwnershipError::OwnedByAnotherProcess { .. }
        ));
        let marker = fs::read(first.lock_path_for_test()).expect("marker bytes");
        assert!(String::from_utf8_lossy(&marker).contains("state=clean"));
        drop(first);
        let second = acquire_fixture_for_test(
            &fixture.descriptor,
            &fixture.registry,
            volume(27),
            "profile-one",
        )
        .expect("restart conservatively reconciles uncertain marker");
        assert_eq!(second.marker.generation, 2);
        drop(second);
        drop(fixture);
    }

    #[test]
    fn prewrite_handoff_failure_fences_retained_context_clones_and_keeps_lock() {
        #[cfg(target_os = "linux")]
        if process_isolation::run_in_child(
            "prewrite_handoff_failure_fences_retained_context_clones_and_keeps_lock",
        ) {
            return;
        }
        let fixture = fixture();
        let mut first = acquire_fixture_for_test(
            &fixture.descriptor,
            &fixture.registry,
            volume(28),
            "profile-one",
        )
        .expect("first volume owner");
        let context = first.try_clone_for_context().expect("context owner clone");
        let error = first
            .test_mark_clean_with_write_failure()
            .expect_err("injected pre-write failure is not a clean receipt");
        assert!(matches!(error, VolumeOwnershipError::MarkerIo { .. }));
        assert!(matches!(
            first.revalidate(),
            Err(VolumeOwnershipError::HandoffFenced { .. })
        ));
        assert!(matches!(
            context.revalidate(),
            Err(VolumeOwnershipError::HandoffFenced { .. })
        ));
        let held = acquire_fixture_for_test(
            &fixture.descriptor,
            &fixture.registry,
            volume(28),
            "profile-one",
        )
        .expect_err("pre-write uncertainty retains the OS lock");
        assert!(matches!(
            held,
            VolumeOwnershipError::OwnedByAnotherProcess { .. }
        ));
        drop(context);
        drop(first);
        let restarted = acquire_fixture_for_test(
            &fixture.descriptor,
            &fixture.registry,
            volume(28),
            "profile-one",
        )
        .expect("dirty marker remains recoverable after failed handoff");
        assert_eq!(restarted.owner_generation(), 1);
        drop(restarted);
        drop(fixture);
    }

    #[test]
    fn malformed_marker_is_rejected_without_replacement() {
        let fixture = fixture();
        let lock_path = fixture.registry.join("volume-24.lock");
        let original = b"stale=untrusted\n";
        fs::write(&lock_path, original).expect("write malformed marker");
        let error = acquire_fixture_for_test(
            &fixture.descriptor,
            &fixture.registry,
            volume(24),
            "profile-one",
        )
        .expect_err("malformed marker must fail closed");
        assert!(matches!(error, VolumeOwnershipError::MarkerCorrupt { .. }));
        assert_eq!(
            fs::read(&lock_path).expect("read malformed marker"),
            original
        );
        drop(fixture);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_volume_uuid_buffer_requires_exact_nonzero_identity() {
        let mut valid = [0_u8; 20];
        valid[..4].copy_from_slice(&20_u32.to_ne_bytes());
        valid[4] = 1;
        assert_eq!(parse_mac_volume_uuid(&valid).unwrap()[0], 1);
        assert!(parse_mac_volume_uuid(&valid[..19]).is_err());
        assert!(parse_mac_volume_uuid(&[valid.as_slice(), &[0]].concat()).is_err());

        let mut truncated_claim = valid;
        truncated_claim[..4].copy_from_slice(&19_u32.to_ne_bytes());
        assert!(parse_mac_volume_uuid(&truncated_claim).is_err());
        let mut oversized_claim = valid;
        oversized_claim[..4].copy_from_slice(&21_u32.to_ne_bytes());
        assert!(parse_mac_volume_uuid(&oversized_claim).is_err());
        let mut zero_uuid = valid;
        zero_uuid[4..].fill(0);
        assert!(parse_mac_volume_uuid(&zero_uuid).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_binding_retains_descriptor_and_rejects_changed_observation() {
        use std::os::unix::fs::MetadataExt;

        let fixture = fixture();
        let volume = descriptor_capacity_facts(&fixture.descriptor)
            .expect("descriptor volume facts")
            .identity;
        let binding =
            MacVolumeBinding::acquire(&fixture.descriptor, &volume).expect("macOS descriptor UUID");
        let retained_path = fixture.directory.path().join("descriptor");
        fs::remove_file(&retained_path).expect("unlink original descriptor path");
        drop(fixture.descriptor);
        assert_eq!(
            binding.descriptor.metadata().unwrap().dev() as u64,
            volume.device
        );
        binding
            .validate(&volume)
            .expect("retained descriptor still identifies volume");

        let changed_uuid = MacVolumeObservation {
            device: volume.device,
            uuid: [0x5a; 16],
        };
        assert!(
            binding
                .validate_with(&volume, |_| Ok(changed_uuid))
                .is_err()
        );
        let changed_device = MacVolumeObservation {
            device: volume.device.wrapping_add(1),
            uuid: binding.uuid,
        };
        assert!(
            binding
                .validate_with(&volume, |_| Ok(changed_device))
                .is_err()
        );
        drop(binding);
        drop(fixture.directory);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_foreign_dirty_v1_marker_reports_ambiguity_and_preserves_bytes() {
        let fixture = fixture();
        let volume = descriptor_capacity_facts(&fixture.descriptor)
            .expect("descriptor volume facts")
            .identity;
        let expected_device = volume.device;
        let binding = Some(
            MacVolumeBinding::acquire(&fixture.descriptor, &volume).expect("macOS descriptor UUID"),
        );
        let marker_path = fixture
            .registry
            .join(lock_file_name(&volume).expect("valid fixture device"));
        let original =
            b"version=1\nstate=dirty\nowner_profile_identity=old-profile\ngeneration=9\n";
        fs::write(&marker_path, original).expect("write old dirty v1 marker");
        let registry = open_no_follow_directory(&fixture.registry).expect("open fixture registry");
        let registry_identity = identity_from_dir(&registry).expect("registry identity");

        let error = acquire_inner_with_binding(
            RegistryRoot {
                path: fixture.registry.clone(),
                identity: registry_identity,
                capability: registry,
            },
            volume,
            String::from("new-profile"),
            false,
            binding,
        )
        .expect_err("foreign dirty marker remains blocked");
        let expected_uuid = format_uuid(&observe_mac_volume(&fixture.descriptor).unwrap().uuid);
        let diagnostic = error.to_string();
        assert!(matches!(
            error,
            VolumeOwnershipError::AmbiguousLegacyDirtyMarker {
                device: actual_device,
                volume_uuid,
                marker_path: actual_path,
                owner_profile_identity,
                generation: 9,
            } if actual_device == expected_device
                && volume_uuid == expected_uuid
                && actual_path == marker_path
                && owner_profile_identity == "old-profile"
        ));
        assert!(diagnostic.contains(&expected_device.to_string()));
        assert!(diagnostic.contains(&expected_uuid));
        assert!(diagnostic.contains(&marker_path.display().to_string()));
        assert!(diagnostic.contains("old-profile"));
        assert!(diagnostic.contains("generation 9"));
        assert!(diagnostic.contains("preserve and reconcile"));
        assert_eq!(fs::read(&marker_path).expect("preserved marker"), original);
        drop(fixture);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_same_profile_dirty_marker_retains_existing_recovery_continuity() {
        let fixture = fixture();
        let volume = descriptor_capacity_facts(&fixture.descriptor)
            .expect("descriptor volume facts")
            .identity;
        let binding = Some(
            MacVolumeBinding::acquire(&fixture.descriptor, &volume).expect("macOS descriptor UUID"),
        );
        let marker_path = fixture
            .registry
            .join(lock_file_name(&volume).expect("valid fixture device"));
        let original =
            b"version=1\nstate=dirty\nowner_profile_identity=same-profile\ngeneration=4\n";
        fs::write(&marker_path, original).expect("write same-profile dirty v1 marker");
        let registry = open_no_follow_directory(&fixture.registry).expect("open fixture registry");
        let registry_identity = identity_from_dir(&registry).expect("registry identity");

        let owner = acquire_inner_with_binding(
            RegistryRoot {
                path: fixture.registry.clone(),
                identity: registry_identity,
                capability: registry,
            },
            volume,
            String::from("same-profile"),
            false,
            binding,
        )
        .expect("same-profile recovery-only continuity");
        assert_eq!(owner.owner_generation(), 4);
        owner.revalidate().expect("retained UUID binding validates");
        assert_eq!(fs::read(&marker_path).expect("unchanged marker"), original);
        drop(owner);
        drop(fixture);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_uuid_binding_survives_context_clone_and_clean_handoff_generation() {
        let fixture = fixture();
        let volume = descriptor_capacity_facts(&fixture.descriptor)
            .expect("descriptor volume facts")
            .identity;
        let registry = || {
            let capability =
                open_no_follow_directory(&fixture.registry).expect("open fixture registry");
            let identity = identity_from_dir(&capability).expect("registry identity");
            RegistryRoot {
                path: fixture.registry.clone(),
                identity,
                capability,
            }
        };
        let mut owner = acquire_inner_with_binding(
            registry(),
            volume.clone(),
            String::from("handoff-profile"),
            false,
            Some(
                MacVolumeBinding::acquire(&fixture.descriptor, &volume)
                    .expect("initial macOS UUID binding"),
            ),
        )
        .expect("initial macOS owner");
        let context = owner.try_clone_for_context().expect("clone UUID binding");
        let uuid = owner
            .mac_volume
            .as_ref()
            .expect("retained macOS binding")
            .uuid;
        assert_eq!(
            context
                .mac_volume
                .as_ref()
                .expect("cloned macOS binding")
                .uuid,
            uuid
        );
        context.revalidate().expect("cloned binding validates");

        owner
            .mark_clean_after_handoff()
            .expect("UUID is revalidated before clean handoff");
        assert!(matches!(
            context.revalidate(),
            Err(VolumeOwnershipError::HandoffFenced { .. })
        ));
        drop(context);
        drop(owner);

        let restarted = acquire_inner_with_binding(
            registry(),
            volume.clone(),
            String::from("handoff-profile"),
            true,
            Some(
                MacVolumeBinding::acquire(&fixture.descriptor, &volume)
                    .expect("restart macOS UUID binding"),
            ),
        )
        .expect("same UUID clean-handoff restart");
        assert_eq!(restarted.owner_generation(), 2);
        assert_eq!(
            restarted.mac_volume.as_ref().unwrap().uuid,
            uuid,
            "clean handoff retains the existing generation semantics on this UUID"
        );
        restarted.revalidate().expect("restarted binding validates");
        drop(restarted);
        drop(fixture);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_changed_expected_uuid_blocks_revalidation_and_preserves_dirty_marker() {
        let fixture = fixture();
        let volume = descriptor_capacity_facts(&fixture.descriptor)
            .expect("descriptor volume facts")
            .identity;
        let mut owner = acquire_inner_with_binding(
            {
                let capability =
                    open_no_follow_directory(&fixture.registry).expect("open fixture registry");
                let identity = identity_from_dir(&capability).expect("registry identity");
                RegistryRoot {
                    path: fixture.registry.clone(),
                    identity,
                    capability,
                }
            },
            volume.clone(),
            String::from("binding-profile"),
            false,
            Some(
                MacVolumeBinding::acquire(&fixture.descriptor, &volume)
                    .expect("macOS UUID binding"),
            ),
        )
        .expect("volume owner");
        let marker_path = owner.lock_path_for_test().to_owned();
        let original = fs::read(&marker_path).expect("dirty marker bytes");
        owner.mac_volume.as_mut().unwrap().uuid = [0x7f; 16];

        assert!(matches!(
            owner.revalidate(),
            Err(VolumeOwnershipError::VolumeIdentityUnavailable)
        ));
        assert!(matches!(
            owner.mark_clean_after_handoff(),
            Err(VolumeOwnershipError::VolumeIdentityUnavailable)
        ));
        assert_eq!(
            fs::read(&marker_path).expect("unchanged dirty marker"),
            original
        );
        drop(owner);
        drop(fixture);
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "run against the disposable APFS descriptor using YIELD_OPT1932_APFS_DESCRIPTOR"]
    fn macos_apfs_volume_uuid_provider_probe() {
        let path = std::env::var_os("YIELD_OPT1932_APFS_DESCRIPTOR")
            .expect("set YIELD_OPT1932_APFS_DESCRIPTOR to a disposable APFS file");
        let descriptor = File::open(path).expect("open disposable APFS descriptor");
        let facts = descriptor_capacity_facts(&descriptor).expect("descriptor capacity facts");
        let observation = observe_mac_volume(&descriptor).expect("APFS UUID observation");
        assert_eq!(observation.device, facts.identity.device);
        assert_ne!(observation.uuid, [0; 16]);
    }

    #[cfg(unix)]
    #[test]
    fn replaced_registry_root_is_rejected_without_touching_entries() {
        use std::os::unix::fs::symlink;

        let fixture = fixture();
        let owner = acquire_fixture_for_test(
            &fixture.descriptor,
            &fixture.registry,
            volume(25),
            "profile-one",
        )
        .expect("volume owner");
        let displaced = fixture.directory.path().join("registry-displaced");
        fs::rename(&fixture.registry, &displaced).expect("displace registry root");
        symlink(&displaced, &fixture.registry).expect("replace registry root with symlink");

        let error = owner
            .revalidate()
            .expect_err("replaced root must be rejected");
        assert!(matches!(
            error,
            VolumeOwnershipError::RootUnavailable { .. }
                | VolumeOwnershipError::RootReplaced { .. }
        ));
        assert!(
            fs::symlink_metadata(&fixture.registry)
                .expect("read replacement root")
                .file_type()
                .is_symlink()
        );

        fs::remove_file(&fixture.registry).expect("remove test symlink");
        fs::rename(displaced, &fixture.registry).expect("restore registry root");
        drop(owner);
        drop(fixture);
    }

    #[cfg(unix)]
    #[test]
    fn replaced_lock_entry_is_rejected_without_touching_replacement() {
        let fixture = fixture();
        let owner = acquire_fixture_for_test(
            &fixture.descriptor,
            &fixture.registry,
            volume(26),
            "profile-one",
        )
        .expect("volume owner");
        let lock_path = owner.lock_path_for_test().to_path_buf();
        let displaced = fixture.directory.path().join("lock-displaced");
        fs::rename(&lock_path, &displaced).expect("displace lock entry");
        let replacement = b"replacement-must-survive\n";
        fs::write(&lock_path, replacement).expect("write replacement lock entry");

        let error = owner
            .revalidate()
            .expect_err("replaced lock must be rejected");
        assert!(matches!(error, VolumeOwnershipError::LockReplaced { .. }));
        assert_eq!(
            fs::read(&lock_path).expect("read replacement lock"),
            replacement
        );

        fs::remove_file(&lock_path).expect("remove test replacement");
        fs::rename(displaced, &lock_path).expect("restore lock entry");
        drop(owner);
        drop(fixture);
    }

    #[cfg(unix)]
    #[test]
    fn cross_process_profiles_contend_without_large_allocation() {
        let fixture = fixture();
        let mut child = Command::new(std::env::current_exe().expect("test executable"))
            .arg("--exact")
            .arg("operation_journal::capacity_volume_owner::tests::volume_owner_process_helper")
            .arg("--ignored")
            .arg("--nocapture")
            .env(VOLUME_OWNER_HELPER_ROOT, &fixture.registry)
            .env(VOLUME_OWNER_HELPER_VOLUME, "27")
            .env(VOLUME_OWNER_HELPER_PROFILE, "profile-process-one")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn volume owner helper");
        let stdout = child.stdout.take().expect("helper stdout");
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            assert!(reader.read_line(&mut line).expect("read helper output") > 0);
            if line.trim_end().ends_with("YIELD_VOLUME_OWNER_READY") {
                break;
            }
        }

        let conflict = acquire_fixture_for_test(
            &fixture.descriptor,
            &fixture.registry,
            volume(27),
            "profile-process-two",
        )
        .expect_err("cross-process same volume must contend");
        assert!(matches!(
            conflict,
            VolumeOwnershipError::OwnedByAnotherProcess { .. }
        ));

        let mut stdin = child.stdin.take().expect("helper stdin");
        writeln!(stdin, "release").expect("release helper");
        drop(stdin);
        let status = child.wait().expect("wait for helper");
        assert!(status.success(), "helper status: {status}");

        let stale = acquire_fixture_for_test(
            &fixture.descriptor,
            &fixture.registry,
            volume(27),
            "profile-process-two",
        )
        .expect_err("released dirty marker must require reconciliation");
        assert!(matches!(
            stale,
            VolumeOwnershipError::ReconciliationRequired {
                owner_profile_identity,
                generation: 1,
            } if owner_profile_identity == "profile-process-one"
        ));
        drop(fixture);
    }

    const VOLUME_OWNER_HELPER_ROOT: &str = "YIELD_VOLUME_OWNER_HELPER_ROOT";
    const VOLUME_OWNER_HELPER_VOLUME: &str = "YIELD_VOLUME_OWNER_HELPER_VOLUME";
    const VOLUME_OWNER_HELPER_PROFILE: &str = "YIELD_VOLUME_OWNER_HELPER_PROFILE";

    #[cfg(unix)]
    #[test]
    #[ignore = "spawned by cross_process_profiles_contend_without_large_allocation"]
    fn volume_owner_process_helper() {
        let registry =
            PathBuf::from(std::env::var(VOLUME_OWNER_HELPER_ROOT).expect("helper registry root"));
        let volume_device = std::env::var(VOLUME_OWNER_HELPER_VOLUME)
            .expect("helper volume")
            .parse::<u64>()
            .expect("helper volume number");
        let profile = std::env::var(VOLUME_OWNER_HELPER_PROFILE).expect("helper profile");
        let descriptor_path = registry
            .parent()
            .expect("helper fixture parent")
            .join("descriptor");
        let descriptor = File::open(descriptor_path).expect("helper descriptor");
        let _owner =
            acquire_fixture_for_test(&descriptor, &registry, volume(volume_device), &profile)
                .expect("helper volume owner");
        println!("YIELD_VOLUME_OWNER_READY");
        io::stdout().flush().expect("flush helper handshake");
        let mut command = String::new();
        io::stdin()
            .read_line(&mut command)
            .expect("read helper command");
        assert_eq!(command.trim(), "release");
    }
}
