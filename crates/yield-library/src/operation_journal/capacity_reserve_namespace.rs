//! Private bootstrap namespace for one owner-created protected reserve.
//!
//! The caller supplies a live [`BoundDatabaseRoot`] for a dedicated directory it already owns.
//! This module binds exactly one direct child, `recovery-reserve.bin`, to that retained root
//! capability. The binding is a physical namespace observation; it is not a per-volume lock,
//! admission claim, durable accounting record, or cross-process ownership registry.

use std::fs::File;
use std::path::Path;

use thiserror::Error;

use crate::app_dirs::WritableProfileGuard;
use crate::durable_capacity::{
    PROTECTED_FREE_SPACE_FLOOR, VolumeFacts, VolumeIdentity, descriptor_capacity_facts,
};
#[cfg(target_os = "macos")]
use crate::filesystem_identity::stable_filesystem_identity_from_open_file;
use crate::sample_sources::db::BoundDatabaseRoot;

use super::capacity_reserve::{ProtectedReserve, ReserveFailureReason, ReserveObservation};
#[cfg(target_os = "macos")]
use super::capacity_reserve::{
    allocate_protected_reserve, revalidate_existing_owned_file,
    revalidate_existing_partial_owned_file,
};

/// Reserved direct child name for the owner-created recovery-reserve namespace.
pub(super) const RESERVE_FILE_NAME: &str = "recovery-reserve.bin";

const MAX_RESERVE_IDENTITY_BYTES: usize = 256;

/// Bounded physical observations needed to reacquire one retained reserve entry.
///
/// This is an in-process observation copy. It is deliberately not a serialized authority and
/// cannot construct a [`ProtectedReserve`] without reopening the fixed child and revalidating
/// facts from its live descriptor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReserveNamespaceObservation {
    root_identity: String,
    file_identity: String,
    volume: VolumeIdentity,
    allocation_unit: u64,
    length: u64,
    allocated_bytes: u64,
}

impl ReserveNamespaceObservation {
    fn from_reserve(root_identity: &str, reserve: &ProtectedReserve) -> Result<Self, &'static str> {
        let observation = reserve.observation();
        let result = Self {
            root_identity: root_identity.to_owned(),
            file_identity: observation.identity.clone(),
            volume: observation.volume.clone(),
            allocation_unit: observation.allocation_unit,
            length: observation.length,
            allocated_bytes: observation.allocated_bytes,
        };
        result.validate_shape()?;
        Ok(result)
    }

    pub(super) fn from_binding(
        root_identity: &str,
        file_identity: &str,
        volume: VolumeIdentity,
        allocation_unit: u64,
    ) -> Result<Self, &'static str> {
        let rounded_floor = round_up_namespace_extent(PROTECTED_FREE_SPACE_FLOOR, allocation_unit)?;
        let result = Self {
            root_identity: root_identity.to_owned(),
            file_identity: file_identity.to_owned(),
            volume,
            allocation_unit,
            length: PROTECTED_FREE_SPACE_FLOOR,
            allocated_bytes: rounded_floor,
        };
        result.validate_shape()?;
        Ok(result)
    }

    fn to_file_observation(&self) -> ReserveObservation {
        ReserveObservation {
            identity: self.file_identity.clone(),
            volume: self.volume.clone(),
            allocation_unit: self.allocation_unit,
            length: self.length,
            allocated_bytes: self.allocated_bytes,
            regular: true,
            links: 1,
            compressed: false,
        }
    }

    fn validate_shape(&self) -> Result<(), &'static str> {
        if self.root_identity.is_empty() || self.root_identity.len() > MAX_RESERVE_IDENTITY_BYTES {
            return Err("reserve root identity is outside its bounded size");
        }
        if self.file_identity.is_empty() || self.file_identity.len() > MAX_RESERVE_IDENTITY_BYTES {
            return Err("reserve file identity is outside its bounded size");
        }
        if self.allocation_unit == 0 {
            return Err("reserve allocation unit is zero");
        }
        if self.length > PROTECTED_FREE_SPACE_FLOOR {
            return Err("reserve length exceeds the protected floor");
        }
        let remainder = self.length % self.allocation_unit;
        let rounded_length = if remainder == 0 {
            self.length
        } else {
            self.length
                .checked_add(self.allocation_unit - remainder)
                .ok_or("reserve length rounding overflowed")?
        };
        if self.allocated_bytes < rounded_length {
            return Err("reserve allocation is below the rounded logical length");
        }
        Ok(())
    }

    pub(super) fn root_identity(&self) -> &str {
        &self.root_identity
    }

    pub(super) fn file_identity(&self) -> &str {
        &self.file_identity
    }

    pub(super) fn volume(&self) -> &VolumeIdentity {
        &self.volume
    }

    pub(super) fn allocation_unit(&self) -> u64 {
        self.allocation_unit
    }

    pub(super) fn length(&self) -> u64 {
        self.length
    }

    pub(super) fn allocated_bytes(&self) -> u64 {
        self.allocated_bytes
    }
}

fn round_up_namespace_extent(bytes: u64, allocation_unit: u64) -> Result<u64, &'static str> {
    if allocation_unit == 0 {
        return Err("reserve allocation unit is zero");
    }
    let remainder = bytes % allocation_unit;
    if remainder == 0 {
        Ok(bytes)
    } else {
        bytes
            .checked_add(allocation_unit - remainder)
            .ok_or("reserve length rounding overflowed")
    }
}

/// Failure stage for namespace creation or reacquisition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ReserveNamespaceFailureStage {
    Unsupported,
    RootValidation,
    Open,
    Allocation,
    Sync,
    Binding,
    Observation,
}

/// Typed namespace failure. Any descriptor that was available remains owned by this value.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(super) enum ReserveNamespaceFailureReason {
    #[error("protected reserve namespace is unsupported on this target")]
    Unsupported,
    #[error("protected reserve root validation failed: {0}")]
    RootValidation(String),
    #[error("protected reserve root identity does not match the stored observation")]
    RootIdentityMismatch,
    #[error("protected reserve child already exists")]
    ChildExists,
    #[error("protected reserve child is missing")]
    MissingChild,
    #[error("protected reserve namespace open failed: {0}")]
    Open(String),
    #[error("protected reserve observation is invalid: {0}")]
    InvalidObservation(&'static str),
    #[error("protected reserve allocation failed: {0}")]
    Allocation(#[source] ReserveFailureReason),
    #[error("protected reserve namespace sync failed: {0}")]
    Sync(String),
    #[error("protected reserve namespace observation failed: {0}")]
    Observation(#[source] ReserveFailureReason),
}

#[derive(Debug)]
enum OwnedFailureFile {
    Protected(ProtectedReserve),
    Raw(File),
}

impl OwnedFailureFile {
    fn into_file(self) -> File {
        match self {
            Self::Protected(reserve) => reserve.into_file(),
            Self::Raw(file) => file,
        }
    }
}

/// A namespace failure retains any file descriptor that was available for owner-directed recovery.
#[derive(Debug)]
pub(super) struct ReserveNamespaceFailure {
    stage: ReserveNamespaceFailureStage,
    reason: ReserveNamespaceFailureReason,
    owned_file: Option<OwnedFailureFile>,
    observation: Option<ReserveNamespaceObservation>,
    observed_allocation: Option<u64>,
}

impl ReserveNamespaceFailure {
    fn new(
        stage: ReserveNamespaceFailureStage,
        reason: ReserveNamespaceFailureReason,
        owned_file: Option<OwnedFailureFile>,
        observation: Option<ReserveNamespaceObservation>,
        observed_allocation: Option<u64>,
    ) -> Self {
        Self {
            stage,
            reason,
            owned_file,
            observation,
            observed_allocation,
        }
    }

    pub(super) fn stage(&self) -> ReserveNamespaceFailureStage {
        self.stage
    }

    pub(super) fn reason(&self) -> &ReserveNamespaceFailureReason {
        &self.reason
    }

    pub(super) fn observation(&self) -> Option<&ReserveNamespaceObservation> {
        self.observation.as_ref()
    }

    pub(super) fn observed_allocation(&self) -> Option<u64> {
        self.observed_allocation
    }

    /// Recover the retained descriptor without deleting or truncating its namespace entry.
    pub(super) fn into_file(self) -> Option<File> {
        self.owned_file.map(OwnedFailureFile::into_file)
    }
}

impl std::fmt::Display for ReserveNamespaceFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "protected reserve namespace {:?}: {}",
            self.stage, self.reason
        )
    }
}

impl std::error::Error for ReserveNamespaceFailure {}

/// A live namespace-bound reserve whose lifetime cannot outlive its caller-owned root capability.
pub(crate) struct BoundReserve<'a> {
    _root: &'a BoundDatabaseRoot,
    _profile_guard: &'a WritableProfileGuard,
    reserve: ProtectedReserve,
    observation: ReserveNamespaceObservation,
}

/// Owner-thread reserve storage that does not borrow the journal store.
///
/// The store keeps the original profile-child root lease alive. This capsule owns a duplicate
/// descriptor and the protected file handle, while [`BoundReserveView`] supplies the short-lived
/// borrowed view needed for descriptor validation or a recovery command.
pub(crate) struct OwnedReserveAllocation {
    root: BoundDatabaseRoot,
    reserve: ProtectedReserve,
    observation: ReserveNamespaceObservation,
}

/// Ephemeral validation view over an owner-thread reserve capsule.
pub(crate) struct BoundReserveView<'a> {
    root: &'a BoundDatabaseRoot,
    profile_guard: &'a WritableProfileGuard,
    reserve: &'a mut ProtectedReserve,
    observation: &'a ReserveNamespaceObservation,
}

/// Fresh, descriptor-derived proof that the retained reserve still owns its physical floor.
///
/// This proof is intentionally non-serialized and can only be produced by revalidating the
/// retained reserve handle. It is accepted by the private ledger context for reconstitution;
/// callers cannot rebuild it from a stored observation.
#[derive(Clone, Debug)]
pub(crate) struct PhysicalReserveProof {
    volume: VolumeIdentity,
    physical_bytes: u64,
    non_sparse: bool,
}

impl PhysicalReserveProof {
    pub(super) fn volume(&self) -> &VolumeIdentity {
        &self.volume
    }

    pub(super) fn physical_bytes(&self) -> u64 {
        self.physical_bytes
    }

    pub(super) fn non_sparse(&self) -> bool {
        self.non_sparse
    }
}

#[cfg(test)]
pub(super) fn test_physical_reserve_proof(
    volume: VolumeIdentity,
    physical_bytes: u64,
    non_sparse: bool,
) -> PhysicalReserveProof {
    PhysicalReserveProof {
        volume,
        physical_bytes,
        non_sparse,
    }
}

impl BoundReserve<'_> {
    pub(super) fn observation(&self) -> &ReserveNamespaceObservation {
        &self.observation
    }

    pub(super) fn profile_identity(&self) -> &str {
        self._profile_guard.profile_root_identity()
    }

    /// Move the non-borrowed reserve state into an owner-thread capsule.
    pub(super) fn into_owned(self) -> Result<OwnedReserveAllocation, ReserveNamespaceFailure> {
        let BoundReserve {
            _root,
            reserve,
            observation,
            ..
        } = self;
        let root = match _root.try_clone_for_owned_capability() {
            Ok(root) => root,
            Err(error) => {
                return Err(ReserveNamespaceFailure::new(
                    ReserveNamespaceFailureStage::Binding,
                    ReserveNamespaceFailureReason::RootValidation(error.to_string()),
                    Some(OwnedFailureFile::Protected(reserve)),
                    Some(observation),
                    None,
                ));
            }
        };
        Ok(OwnedReserveAllocation {
            root,
            reserve,
            observation,
        })
    }

    pub(super) fn fresh_capacity_facts(
        &self,
    ) -> Result<VolumeFacts, ReserveNamespaceFailureReason> {
        #[cfg(not(target_os = "macos"))]
        {
            return Err(ReserveNamespaceFailureReason::Unsupported);
        }
        #[cfg(target_os = "macos")]
        {
            let facts = descriptor_capacity_facts(self.reserve.file()).map_err(|error| {
                ReserveNamespaceFailureReason::Observation(
                    ReserveFailureReason::CapacityFactsUnavailable(error.to_string()),
                )
            })?;
            if facts.identity != *self.observation.volume() {
                return Err(ReserveNamespaceFailureReason::Observation(
                    ReserveFailureReason::VerifiedVolumeMismatch,
                ));
            }
            if facts.allocation_unit != self.observation.allocation_unit() {
                return Err(ReserveNamespaceFailureReason::Observation(
                    ReserveFailureReason::VerifiedAllocationUnitMismatch,
                ));
            }
            Ok(facts)
        }
    }

    pub(super) fn revalidate_and_prove(
        &self,
    ) -> Result<PhysicalReserveProof, ReserveNamespaceFailureReason> {
        self.revalidate()?;
        Ok(PhysicalReserveProof {
            volume: self.observation.volume().clone(),
            physical_bytes: self.reserve.observation().allocated_bytes,
            non_sparse: true,
        })
    }

    #[cfg(target_os = "macos")]
    pub(super) fn revalidate_partial(
        &self,
    ) -> Result<ReserveObservation, ReserveNamespaceFailureReason> {
        validate_root(self._root, self._profile_guard)?;
        let observed = self
            .reserve
            .revalidate_partial()
            .map_err(ReserveNamespaceFailureReason::Observation)?;
        verify_current_child_binding(self._root, &self.reserve.observation().identity)?;
        validate_root(self._root, self._profile_guard)?;
        Ok(observed)
    }

    #[cfg(target_os = "macos")]
    pub(super) fn revalidate_extent(
        &self,
        expected_length: u64,
    ) -> Result<ReserveObservation, ReserveNamespaceFailureReason> {
        validate_root(self._root, self._profile_guard)?;
        let observed = self
            .reserve
            .revalidate_extent(expected_length)
            .map_err(ReserveNamespaceFailureReason::Observation)?;
        verify_current_child_binding(self._root, &self.reserve.observation().identity)?;
        validate_root(self._root, self._profile_guard)?;
        Ok(observed)
    }

    #[cfg(target_os = "macos")]
    pub(super) fn shrink_to(
        &mut self,
        expected_current_length: u64,
        target_length: u64,
    ) -> Result<ReserveObservation, ReserveNamespaceFailureReason> {
        self.revalidate_extent(expected_current_length)?;
        let observed = self
            .reserve
            .shrink_to(expected_current_length, target_length)
            .map_err(ReserveNamespaceFailureReason::Observation)?;
        verify_current_child_binding(self._root, &self.reserve.observation().identity)?;
        validate_root(self._root, self._profile_guard)?;
        Ok(observed)
    }

    #[cfg(target_os = "macos")]
    pub(super) fn shrink_to_with_mutation_fault(
        &mut self,
        expected_current_length: u64,
        target_length: u64,
        fault: super::capacity_reserve::MutationFault,
    ) -> Result<ReserveObservation, ReserveNamespaceFailureReason> {
        self.revalidate_extent(expected_current_length)?;
        let observed = self
            .reserve
            .shrink_to_with_mutation_fault(expected_current_length, target_length, fault)
            .map_err(ReserveNamespaceFailureReason::Observation)?;
        verify_current_child_binding(self._root, &self.reserve.observation().identity)?;
        validate_root(self._root, self._profile_guard)?;
        Ok(observed)
    }

    #[cfg(target_os = "macos")]
    pub(super) fn ensure_extent_at_most(
        &mut self,
        target_length: u64,
    ) -> Result<ReserveObservation, ReserveNamespaceFailureReason> {
        self.revalidate_partial()?;
        let observed = self
            .reserve
            .ensure_extent_at_most(target_length)
            .map_err(ReserveNamespaceFailureReason::Observation)?;
        verify_current_child_binding(self._root, &self.reserve.observation().identity)?;
        validate_root(self._root, self._profile_guard)?;
        Ok(observed)
    }

    #[cfg(target_os = "macos")]
    pub(super) fn ensure_extent_at_most_with_mutation_fault(
        &mut self,
        target_length: u64,
        fault: super::capacity_reserve::MutationFault,
    ) -> Result<ReserveObservation, ReserveNamespaceFailureReason> {
        self.revalidate_partial()?;
        let observed = self
            .reserve
            .ensure_extent_at_most_with_mutation_fault(target_length, fault)
            .map_err(ReserveNamespaceFailureReason::Observation)?;
        verify_current_child_binding(self._root, &self.reserve.observation().identity)?;
        validate_root(self._root, self._profile_guard)?;
        Ok(observed)
    }

    #[cfg(target_os = "macos")]
    pub(super) fn reconstitute_to_floor(
        &mut self,
    ) -> Result<ReserveObservation, ReserveNamespaceFailureReason> {
        self.revalidate_partial()?;
        let observed = self
            .reserve
            .reconstitute_to_floor()
            .map_err(ReserveNamespaceFailureReason::Observation)?;
        verify_current_child_binding(self._root, &self.reserve.observation().identity)?;
        validate_root(self._root, self._profile_guard)?;
        Ok(observed)
    }

    #[cfg(target_os = "macos")]
    pub(super) fn reconstitute_to_floor_with_mutation_fault(
        &mut self,
        fault: super::capacity_reserve::MutationFault,
    ) -> Result<ReserveObservation, ReserveNamespaceFailureReason> {
        self.revalidate_partial()?;
        let observed = self
            .reserve
            .reconstitute_to_floor_with_mutation_fault(fault)
            .map_err(ReserveNamespaceFailureReason::Observation)?;
        verify_current_child_binding(self._root, &self.reserve.observation().identity)?;
        validate_root(self._root, self._profile_guard)?;
        Ok(observed)
    }

    /// Reopen the fixed child through the retained root and verify it against live descriptor facts.
    pub(super) fn revalidate(&self) -> Result<(), ReserveNamespaceFailureReason> {
        #[cfg(not(target_os = "macos"))]
        {
            return Err(ReserveNamespaceFailureReason::Unsupported);
        }
        #[cfg(target_os = "macos")]
        {
            validate_root(self._root, self._profile_guard)?;
            self.reserve
                .revalidate()
                .map_err(ReserveNamespaceFailureReason::Observation)?;
            let file = open_existing(self._root)?;
            let expected = self.reserve.observation().clone();
            let rebound = match revalidate_existing_owned_file(file, &expected) {
                Ok(reserve) => reserve,
                Err(failure) => {
                    let reason = failure.reason().clone();
                    drop(failure.into_file());
                    return Err(ReserveNamespaceFailureReason::Observation(reason));
                }
            };
            drop(rebound);
            verify_current_child_binding(self._root, &expected.identity)?;
            validate_root(self._root, self._profile_guard)
        }
    }
}

impl OwnedReserveAllocation {
    pub(super) fn observation(&self) -> &ReserveNamespaceObservation {
        &self.observation
    }

    pub(super) fn view<'a>(
        &'a mut self,
        profile_guard: &'a WritableProfileGuard,
    ) -> BoundReserveView<'a> {
        BoundReserveView {
            root: &self.root,
            profile_guard,
            reserve: &mut self.reserve,
            observation: &self.observation,
        }
    }
}

impl BoundReserveView<'_> {
    pub(super) fn observation(&self) -> &ReserveNamespaceObservation {
        self.observation
    }

    /// Return fresh facts from the retained reserve descriptor after checking its entire
    /// descriptor-bound namespace.  This is an observation only: it does not create a reserve,
    /// mutate the namespace, or grant ordinary capacity authority.
    pub(super) fn fresh_capacity_facts(
        &self,
    ) -> Result<VolumeFacts, ReserveNamespaceFailureReason> {
        // A valid active recovery charge may leave the retained reserve shorter than its full
        // bootstrap extent.  The owner already checked that charge-derived minimum before this
        // observation, so use the partial revalidation boundary rather than requiring a fresh
        // full-floor extent here.
        self.revalidate_partial_and_prove()?;
        let facts = descriptor_capacity_facts(self.reserve.file()).map_err(|error| {
            ReserveNamespaceFailureReason::Observation(
                ReserveFailureReason::CapacityFactsUnavailable(error.to_string()),
            )
        })?;
        if facts.identity != *self.observation.volume() {
            return Err(ReserveNamespaceFailureReason::Observation(
                ReserveFailureReason::VerifiedVolumeMismatch,
            ));
        }
        if facts.allocation_unit != self.observation.allocation_unit() {
            return Err(ReserveNamespaceFailureReason::Observation(
                ReserveFailureReason::VerifiedAllocationUnitMismatch,
            ));
        }
        self.revalidate_partial_and_prove()?;
        Ok(facts)
    }

    pub(super) fn revalidate(&self) -> Result<(), ReserveNamespaceFailureReason> {
        #[cfg(not(target_os = "macos"))]
        {
            return Err(ReserveNamespaceFailureReason::Observation(
                ReserveFailureReason::Unsupported,
            ));
        }
        #[cfg(target_os = "macos")]
        {
            validate_root(self.root, self.profile_guard)?;
            self.reserve
                .revalidate()
                .map_err(ReserveNamespaceFailureReason::Observation)?;
            verify_current_child_binding(self.root, &self.reserve.observation().identity)?;
            validate_root(self.root, self.profile_guard)
        }
    }

    pub(super) fn revalidate_and_prove(
        &self,
    ) -> Result<PhysicalReserveProof, ReserveNamespaceFailureReason> {
        self.revalidate()?;
        Ok(PhysicalReserveProof {
            volume: self.observation.volume().clone(),
            physical_bytes: self.reserve.observation().allocated_bytes,
            non_sparse: true,
        })
    }

    /// Revalidate a reserve that has been safely shortened by the durable active-charge budget.
    /// The proof still comes from the live descriptor and remains non-serializable; callers must
    /// compare its allocation with the charge-derived minimum before using it as a boundary.
    pub(super) fn revalidate_partial_and_prove(
        &self,
    ) -> Result<PhysicalReserveProof, ReserveNamespaceFailureReason> {
        #[cfg(not(target_os = "macos"))]
        {
            return Err(ReserveNamespaceFailureReason::Observation(
                super::capacity_reserve::ReserveFailureReason::Unsupported,
            ));
        }
        #[cfg(target_os = "macos")]
        {
            validate_root(self.root, self.profile_guard)?;
            let observed = self
                .reserve
                .revalidate_partial()
                .map_err(ReserveNamespaceFailureReason::Observation)?;
            verify_current_child_binding(self.root, &self.reserve.observation().identity)?;
            validate_root(self.root, self.profile_guard)?;
            Ok(PhysicalReserveProof {
                volume: observed.volume,
                physical_bytes: observed.allocated_bytes,
                non_sparse: true,
            })
        }
    }
}

/// Create the fixed reserve child under an already-owned, live root capability.
pub(super) fn create_owned_reserve<'a>(
    root: &'a BoundDatabaseRoot,
    profile_guard: &'a WritableProfileGuard,
) -> Result<BoundReserve<'a>, ReserveNamespaceFailure> {
    #[cfg(not(target_os = "macos"))]
    {
        // Keep this check first: unsupported targets must not create or inspect a namespace entry.
        let _ = (root, profile_guard);
        return Err(unsupported_failure());
    }
    #[cfg(target_os = "macos")]
    {
        create_owned_reserve_inner(root, profile_guard, |root, profile_guard| {
            root.sync_namespace(profile_guard)
                .map_err(|error| error.to_string())
        })
    }
}

#[cfg(target_os = "macos")]
fn create_owned_reserve_inner<'a, Sync>(
    root: &'a BoundDatabaseRoot,
    profile_guard: &'a WritableProfileGuard,
    sync: Sync,
) -> Result<BoundReserve<'a>, ReserveNamespaceFailure>
where
    Sync: FnOnce(&BoundDatabaseRoot, &WritableProfileGuard) -> Result<(), String>,
{
    validate_root(root, profile_guard).map_err(|reason| {
        ReserveNamespaceFailure::new(
            ReserveNamespaceFailureStage::RootValidation,
            reason,
            None,
            None,
            None,
        )
    })?;
    validate_root_identity(root.identity()).map_err(|reason| {
        ReserveNamespaceFailure::new(
            ReserveNamespaceFailureStage::Observation,
            reason,
            None,
            None,
            None,
        )
    })?;

    let file = open_new(root).map_err(|reason| {
        ReserveNamespaceFailure::new(ReserveNamespaceFailureStage::Open, reason, None, None, None)
    })?;
    let reserve = match allocate_protected_reserve(file) {
        Ok(reserve) => reserve,
        Err(failure) => {
            let reason = failure.reason().clone();
            let observed_allocation = failure.observed_allocation();
            let file = failure.into_file();
            return Err(ReserveNamespaceFailure::new(
                ReserveNamespaceFailureStage::Allocation,
                ReserveNamespaceFailureReason::Allocation(reason),
                Some(OwnedFailureFile::Raw(file)),
                None,
                observed_allocation,
            ));
        }
    };
    finish_created_reserve(root, profile_guard, reserve, sync)
}

#[cfg(target_os = "macos")]
fn finish_created_reserve<'a, Sync>(
    root: &'a BoundDatabaseRoot,
    profile_guard: &'a WritableProfileGuard,
    reserve: ProtectedReserve,
    sync: Sync,
) -> Result<BoundReserve<'a>, ReserveNamespaceFailure>
where
    Sync: FnOnce(&BoundDatabaseRoot, &WritableProfileGuard) -> Result<(), String>,
{
    let observation = match ReserveNamespaceObservation::from_reserve(root.identity(), &reserve) {
        Ok(observation) => observation,
        Err(reason) => {
            return Err(ReserveNamespaceFailure::new(
                ReserveNamespaceFailureStage::Observation,
                ReserveNamespaceFailureReason::InvalidObservation(reason),
                Some(OwnedFailureFile::Protected(reserve)),
                None,
                None,
            ));
        }
    };

    if let Err(error) = sync(root, profile_guard) {
        return Err(ReserveNamespaceFailure::new(
            ReserveNamespaceFailureStage::Sync,
            ReserveNamespaceFailureReason::Sync(error),
            Some(OwnedFailureFile::Protected(reserve)),
            Some(observation),
            None,
        ));
    }
    if let Err(reason) = validate_root(root, profile_guard) {
        return Err(ReserveNamespaceFailure::new(
            ReserveNamespaceFailureStage::Binding,
            reason,
            Some(OwnedFailureFile::Protected(reserve)),
            Some(observation),
            None,
        ));
    }

    bind_after_sync(root, profile_guard, reserve, observation)
}

#[cfg(test)]
#[cfg(target_os = "macos")]
fn create_owned_reserve_with_test_sync<'a, Sync>(
    root: &'a BoundDatabaseRoot,
    profile_guard: &'a WritableProfileGuard,
    sync: Sync,
) -> Result<BoundReserve<'a>, ReserveNamespaceFailure>
where
    Sync: FnOnce(&BoundDatabaseRoot, &WritableProfileGuard) -> Result<(), String>,
{
    validate_root(root, profile_guard).expect("test fixture root validation");
    validate_root_identity(root.identity()).expect("test fixture root identity");
    let file = open_new(root).expect("test fixture reserve child");
    let reserve = super::capacity_reserve::test_fixture_reserve(file);
    finish_created_reserve(root, profile_guard, reserve, sync)
}

/// Reopen the fixed reserve child from a live root and a bounded, untrusted observation.
pub(super) fn reopen_owned_reserve<'a>(
    root: &'a BoundDatabaseRoot,
    profile_guard: &'a WritableProfileGuard,
    expected: &ReserveNamespaceObservation,
) -> Result<BoundReserve<'a>, ReserveNamespaceFailure> {
    #[cfg(not(target_os = "macos"))]
    {
        // Keep this check first: unsupported targets must not inspect or create a namespace entry.
        let _ = (root, profile_guard, expected);
        return Err(unsupported_failure());
    }
    #[cfg(target_os = "macos")]
    {
        reopen_owned_reserve_impl(root, profile_guard, expected, || {})
    }
}

/// Reopen the fixed reserve child for crash recovery while accepting a descriptor extent that was
/// durably spent. The caller supplies the minimum logical extent derived from the durable ledger;
/// this helper never constructs that authority from the stored observation itself.
pub(super) fn reopen_owned_reserve_partial<'a>(
    root: &'a BoundDatabaseRoot,
    profile_guard: &'a WritableProfileGuard,
    expected: &ReserveNamespaceObservation,
    minimum_length: u64,
) -> Result<BoundReserve<'a>, ReserveNamespaceFailure> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (root, profile_guard, expected, minimum_length);
        return Err(unsupported_failure());
    }
    #[cfg(target_os = "macos")]
    {
        reopen_owned_reserve_partial_impl(root, profile_guard, expected, minimum_length)
    }
}

#[cfg(target_os = "macos")]
fn reopen_owned_reserve_partial_impl<'a>(
    root: &'a BoundDatabaseRoot,
    profile_guard: &'a WritableProfileGuard,
    expected: &ReserveNamespaceObservation,
    minimum_length: u64,
) -> Result<BoundReserve<'a>, ReserveNamespaceFailure> {
    expected.validate_shape().map_err(|reason| {
        ReserveNamespaceFailure::new(
            ReserveNamespaceFailureStage::Observation,
            ReserveNamespaceFailureReason::InvalidObservation(reason),
            None,
            None,
            None,
        )
    })?;
    if minimum_length > PROTECTED_FREE_SPACE_FLOOR {
        return Err(ReserveNamespaceFailure::new(
            ReserveNamespaceFailureStage::Observation,
            ReserveNamespaceFailureReason::InvalidObservation(
                "reserve minimum extent exceeds the protected floor",
            ),
            None,
            None,
            None,
        ));
    }
    validate_root(root, profile_guard).map_err(|reason| {
        ReserveNamespaceFailure::new(
            ReserveNamespaceFailureStage::RootValidation,
            reason,
            None,
            None,
            None,
        )
    })?;
    if root.identity() != expected.root_identity() {
        return Err(ReserveNamespaceFailure::new(
            ReserveNamespaceFailureStage::Binding,
            ReserveNamespaceFailureReason::RootIdentityMismatch,
            None,
            None,
            None,
        ));
    }

    let file = open_existing(root).map_err(|reason| {
        ReserveNamespaceFailure::new(ReserveNamespaceFailureStage::Open, reason, None, None, None)
    })?;
    let expected_file = expected.to_file_observation();
    let reserve = match revalidate_existing_partial_owned_file(file, &expected_file) {
        Ok(reserve) => reserve,
        Err(failure) => {
            let reason = failure.reason().clone();
            let observed_allocation = failure.observed_allocation();
            let file = failure.into_file();
            return Err(ReserveNamespaceFailure::new(
                ReserveNamespaceFailureStage::Binding,
                ReserveNamespaceFailureReason::Observation(reason),
                Some(OwnedFailureFile::Raw(file)),
                None,
                observed_allocation,
            ));
        }
    };
    let observed_length = reserve.observation().length;
    let observed_allocation = reserve.observation().allocated_bytes;
    if observed_length < minimum_length {
        return Err(ReserveNamespaceFailure::new(
            ReserveNamespaceFailureStage::Binding,
            ReserveNamespaceFailureReason::Observation(ReserveFailureReason::LengthMismatch {
                expected: minimum_length,
                actual: observed_length,
            }),
            Some(OwnedFailureFile::Protected(reserve)),
            None,
            Some(observed_allocation),
        ));
    }
    if let Err(reason) = reserve.revalidate_partial() {
        return Err(ReserveNamespaceFailure::new(
            ReserveNamespaceFailureStage::Binding,
            ReserveNamespaceFailureReason::Observation(reason),
            Some(OwnedFailureFile::Protected(reserve)),
            None,
            None,
        ));
    }
    if let Err(reason) = verify_current_child_binding(root, &expected.file_identity) {
        return Err(ReserveNamespaceFailure::new(
            ReserveNamespaceFailureStage::Binding,
            reason,
            Some(OwnedFailureFile::Protected(reserve)),
            None,
            None,
        ));
    }
    let observation = match ReserveNamespaceObservation::from_reserve(root.identity(), &reserve) {
        Ok(observation) => observation,
        Err(reason) => {
            return Err(ReserveNamespaceFailure::new(
                ReserveNamespaceFailureStage::Observation,
                ReserveNamespaceFailureReason::InvalidObservation(reason),
                Some(OwnedFailureFile::Protected(reserve)),
                None,
                None,
            ));
        }
    };
    if let Err(reason) = validate_root(root, profile_guard) {
        return Err(ReserveNamespaceFailure::new(
            ReserveNamespaceFailureStage::Binding,
            reason,
            Some(OwnedFailureFile::Protected(reserve)),
            Some(observation),
            None,
        ));
    }
    Ok(BoundReserve {
        _root: root,
        _profile_guard: profile_guard,
        reserve,
        observation,
    })
}

#[cfg(test)]
#[cfg(target_os = "macos")]
fn reopen_owned_reserve_with_test_hook<'a, Hook>(
    root: &'a BoundDatabaseRoot,
    profile_guard: &'a WritableProfileGuard,
    expected: &ReserveNamespaceObservation,
    before_final_child_check: Hook,
) -> Result<BoundReserve<'a>, ReserveNamespaceFailure>
where
    Hook: FnOnce(),
{
    reopen_owned_reserve_impl(root, profile_guard, expected, before_final_child_check)
}

#[cfg(target_os = "macos")]
fn reopen_owned_reserve_impl<'a, Hook>(
    root: &'a BoundDatabaseRoot,
    profile_guard: &'a WritableProfileGuard,
    expected: &ReserveNamespaceObservation,
    before_final_child_check: Hook,
) -> Result<BoundReserve<'a>, ReserveNamespaceFailure>
where
    Hook: FnOnce(),
{
    expected.validate_shape().map_err(|reason| {
        ReserveNamespaceFailure::new(
            ReserveNamespaceFailureStage::Observation,
            ReserveNamespaceFailureReason::InvalidObservation(reason),
            None,
            None,
            None,
        )
    })?;
    validate_root(root, profile_guard).map_err(|reason| {
        ReserveNamespaceFailure::new(
            ReserveNamespaceFailureStage::RootValidation,
            reason,
            None,
            None,
            None,
        )
    })?;
    if root.identity() != expected.root_identity() {
        return Err(ReserveNamespaceFailure::new(
            ReserveNamespaceFailureStage::Binding,
            ReserveNamespaceFailureReason::RootIdentityMismatch,
            None,
            None,
            None,
        ));
    }

    let file = open_existing(root).map_err(|reason| {
        ReserveNamespaceFailure::new(ReserveNamespaceFailureStage::Open, reason, None, None, None)
    })?;
    let expected_file = expected.to_file_observation();
    let reserve = match revalidate_existing_owned_file(file, &expected_file) {
        Ok(reserve) => reserve,
        Err(failure) => {
            let reason = failure.reason().clone();
            let observed_allocation = failure.observed_allocation();
            let file = failure.into_file();
            return Err(ReserveNamespaceFailure::new(
                ReserveNamespaceFailureStage::Binding,
                ReserveNamespaceFailureReason::Observation(reason),
                Some(OwnedFailureFile::Raw(file)),
                None,
                observed_allocation,
            ));
        }
    };
    if let Err(reason) = reserve.revalidate() {
        return Err(ReserveNamespaceFailure::new(
            ReserveNamespaceFailureStage::Binding,
            ReserveNamespaceFailureReason::Observation(reason),
            Some(OwnedFailureFile::Protected(reserve)),
            None,
            None,
        ));
    }
    before_final_child_check();
    if let Err(reason) = verify_current_child_binding(root, &expected.file_identity) {
        return Err(ReserveNamespaceFailure::new(
            ReserveNamespaceFailureStage::Binding,
            reason,
            Some(OwnedFailureFile::Protected(reserve)),
            None,
            None,
        ));
    }
    let observation = match ReserveNamespaceObservation::from_reserve(root.identity(), &reserve) {
        Ok(observation) => observation,
        Err(reason) => {
            return Err(ReserveNamespaceFailure::new(
                ReserveNamespaceFailureStage::Observation,
                ReserveNamespaceFailureReason::InvalidObservation(reason),
                Some(OwnedFailureFile::Protected(reserve)),
                None,
                None,
            ));
        }
    };
    if let Err(reason) = validate_root(root, profile_guard) {
        return Err(ReserveNamespaceFailure::new(
            ReserveNamespaceFailureStage::Binding,
            reason,
            Some(OwnedFailureFile::Protected(reserve)),
            Some(observation),
            None,
        ));
    }
    Ok(BoundReserve {
        _root: root,
        _profile_guard: profile_guard,
        reserve,
        observation,
    })
}

#[cfg(target_os = "macos")]
fn bind_after_sync<'a>(
    root: &'a BoundDatabaseRoot,
    profile_guard: &'a WritableProfileGuard,
    reserve: ProtectedReserve,
    observation: ReserveNamespaceObservation,
) -> Result<BoundReserve<'a>, ReserveNamespaceFailure> {
    bind_after_sync_impl(root, profile_guard, reserve, observation, || {})
}

#[cfg(test)]
#[cfg(target_os = "macos")]
fn bind_after_sync_with_test_hook<'a, Hook>(
    root: &'a BoundDatabaseRoot,
    profile_guard: &'a WritableProfileGuard,
    reserve: ProtectedReserve,
    observation: ReserveNamespaceObservation,
    before_final_child_check: Hook,
) -> Result<BoundReserve<'a>, ReserveNamespaceFailure>
where
    Hook: FnOnce(),
{
    bind_after_sync_impl(
        root,
        profile_guard,
        reserve,
        observation,
        before_final_child_check,
    )
}

#[cfg(target_os = "macos")]
fn bind_after_sync_impl<'a, Hook>(
    root: &'a BoundDatabaseRoot,
    profile_guard: &'a WritableProfileGuard,
    reserve: ProtectedReserve,
    observation: ReserveNamespaceObservation,
    before_final_child_check: Hook,
) -> Result<BoundReserve<'a>, ReserveNamespaceFailure>
where
    Hook: FnOnce(),
{
    let expected = reserve.observation().clone();
    let file = match open_existing(root) {
        Ok(file) => file,
        Err(reason) => {
            return Err(ReserveNamespaceFailure::new(
                ReserveNamespaceFailureStage::Binding,
                reason,
                Some(OwnedFailureFile::Protected(reserve)),
                Some(observation),
                None,
            ));
        }
    };
    let rebound = match revalidate_existing_owned_file(file, &expected) {
        Ok(reserve) => reserve,
        Err(failure) => {
            let reason = failure.reason().clone();
            drop(failure.into_file());
            return Err(ReserveNamespaceFailure::new(
                ReserveNamespaceFailureStage::Binding,
                ReserveNamespaceFailureReason::Observation(reason),
                Some(OwnedFailureFile::Protected(reserve)),
                Some(observation),
                None,
            ));
        }
    };
    if let Err(reason) = reserve.revalidate() {
        drop(rebound);
        return Err(ReserveNamespaceFailure::new(
            ReserveNamespaceFailureStage::Binding,
            ReserveNamespaceFailureReason::Observation(reason),
            Some(OwnedFailureFile::Protected(reserve)),
            Some(observation),
            None,
        ));
    }
    before_final_child_check();
    if let Err(reason) = verify_current_child_binding(root, &expected.identity) {
        drop(rebound);
        return Err(ReserveNamespaceFailure::new(
            ReserveNamespaceFailureStage::Binding,
            reason,
            Some(OwnedFailureFile::Protected(reserve)),
            Some(observation),
            None,
        ));
    }
    if let Err(reason) = validate_root(root, profile_guard) {
        drop(rebound);
        return Err(ReserveNamespaceFailure::new(
            ReserveNamespaceFailureStage::Binding,
            reason,
            Some(OwnedFailureFile::Protected(reserve)),
            Some(observation),
            None,
        ));
    }
    drop(reserve);
    let observation = match ReserveNamespaceObservation::from_reserve(root.identity(), &rebound) {
        Ok(observation) => observation,
        Err(reason) => {
            return Err(ReserveNamespaceFailure::new(
                ReserveNamespaceFailureStage::Observation,
                ReserveNamespaceFailureReason::InvalidObservation(reason),
                Some(OwnedFailureFile::Protected(rebound)),
                None,
                None,
            ));
        }
    };
    Ok(BoundReserve {
        _root: root,
        _profile_guard: profile_guard,
        reserve: rebound,
        observation,
    })
}

fn unsupported_failure() -> ReserveNamespaceFailure {
    ReserveNamespaceFailure::new(
        ReserveNamespaceFailureStage::Unsupported,
        ReserveNamespaceFailureReason::Unsupported,
        None,
        None,
        None,
    )
}

fn validate_root(
    root: &BoundDatabaseRoot,
    profile_guard: &WritableProfileGuard,
) -> Result<(), ReserveNamespaceFailureReason> {
    root.validate_profile_guard(profile_guard)
        .map_err(|error| ReserveNamespaceFailureReason::RootValidation(error.to_string()))
}

fn validate_root_identity(identity: &str) -> Result<(), ReserveNamespaceFailureReason> {
    if identity.is_empty() || identity.len() > MAX_RESERVE_IDENTITY_BYTES {
        Err(ReserveNamespaceFailureReason::InvalidObservation(
            "reserve root identity is outside its bounded size",
        ))
    } else {
        Ok(())
    }
}

fn open_new(root: &BoundDatabaseRoot) -> Result<File, ReserveNamespaceFailureReason> {
    root.open_file_nofollow(Path::new(RESERVE_FILE_NAME), true)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                ReserveNamespaceFailureReason::ChildExists
            } else {
                ReserveNamespaceFailureReason::Open(error.to_string())
            }
        })
}

fn open_existing(root: &BoundDatabaseRoot) -> Result<File, ReserveNamespaceFailureReason> {
    root.open_file_nofollow(Path::new(RESERVE_FILE_NAME), false)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ReserveNamespaceFailureReason::MissingChild
            } else {
                ReserveNamespaceFailureReason::Open(error.to_string())
            }
        })
}

#[cfg(target_os = "macos")]
fn verify_current_child_binding(
    root: &BoundDatabaseRoot,
    expected_identity: &str,
) -> Result<(), ReserveNamespaceFailureReason> {
    let file = open_existing(root)?;
    let identity = stable_filesystem_identity_from_open_file(&file).ok_or(
        ReserveNamespaceFailureReason::Observation(ReserveFailureReason::IdentityUnavailable),
    )?;
    if identity != expected_identity {
        return Err(ReserveNamespaceFailureReason::Observation(
            ReserveFailureReason::IdentityMismatch,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    #[cfg(target_os = "macos")]
    use std::fs::OpenOptions;
    #[cfg(target_os = "macos")]
    use std::io::Write;

    use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard};
    use crate::durable_capacity::VolumeIdentity;
    use crate::test_runtime::TestRuntimeGuard;

    struct Fixture {
        owner: WritableProfileGuard,
        _profile: PersistenceProfileGuard,
        _config: ConfigBaseGuard,
        _base: tempfile::TempDir,
        _runtime: TestRuntimeGuard,
    }

    fn fixture() -> Fixture {
        let runtime = TestRuntimeGuard::acquire();
        let base = tempfile::tempdir().expect("temporary config base");
        let base_path = fs::canonicalize(base.path()).expect("canonical temporary config base");
        let config = ConfigBaseGuard::set(base_path);
        let profile = PersistenceProfileGuard::named(format!(
            "reserve-namespace-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let owner = WritableProfileGuard::acquire_current().expect("profile owner");
        Fixture {
            owner,
            _profile: profile,
            _config: config,
            _base: base,
            _runtime: runtime,
        }
    }

    fn root(fixture: &Fixture, name: &str) -> BoundDatabaseRoot {
        BoundDatabaseRoot::for_profile_child(&fixture.owner, Path::new(name))
            .expect("dedicated reserve root")
    }

    #[test]
    fn owned_root_clone_retains_profile_child_lease_until_clone_drop() {
        let fixture = fixture();
        let root = root(&fixture, "reserve-root-lease");
        let clone = root
            .try_clone_for_owned_capability()
            .expect("clone retained root capability");
        drop(root);

        let conflict = match BoundDatabaseRoot::for_profile_child(
            &fixture.owner,
            Path::new("reserve-root-lease"),
        ) {
            Ok(_) => panic!("owned clone must retain the profile-child lease"),
            Err(error) => error,
        };
        assert!(conflict.to_string().contains("already open"));

        drop(clone);
        BoundDatabaseRoot::for_profile_child(&fixture.owner, Path::new("reserve-root-lease"))
            .expect("profile-child lease released with the final clone");
    }

    fn synthetic_observation(
        root_identity: &str,
        file_identity: &str,
    ) -> ReserveNamespaceObservation {
        ReserveNamespaceObservation {
            root_identity: root_identity.to_owned(),
            file_identity: file_identity.to_owned(),
            volume: VolumeIdentity { device: 1 },
            allocation_unit: 1,
            length: PROTECTED_FREE_SPACE_FLOOR,
            allocated_bytes: PROTECTED_FREE_SPACE_FLOOR,
        }
    }

    #[cfg(target_os = "macos")]
    fn allocation_bytes(path: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;

        fs::metadata(path)
            .expect("reserve metadata")
            .blocks()
            .checked_mul(512)
            .expect("reserve allocation arithmetic")
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn create_drop_reopen_preserves_binding_and_rejects_collision() {
        let fixture = fixture();
        let root = root(&fixture, "reserve-root-roundtrip");
        let path = root.path().join(RESERVE_FILE_NAME);

        let reserve = create_owned_reserve(&root, &fixture.owner).expect("create reserve");
        let expected = reserve.observation().clone();
        let expected_allocation = expected.allocated_bytes();
        assert_eq!(expected.length(), PROTECTED_FREE_SPACE_FLOOR);
        assert!(expected_allocation >= PROTECTED_FREE_SPACE_FLOOR);
        let expected_identity = expected.file_identity().to_owned();
        let expected_volume = expected.volume().clone();
        let before_len = fs::metadata(&path).expect("reserve metadata").len();
        let before_allocation = allocation_bytes(&path);
        drop(reserve);

        let reopened = reopen_owned_reserve(&root, &fixture.owner, &expected)
            .expect("explicit reserve reopen");
        assert_eq!(reopened.observation().file_identity(), expected_identity);
        assert_eq!(reopened.observation().volume(), &expected_volume);
        assert_eq!(reopened.observation().length(), before_len);
        assert_eq!(
            reopened.observation().allocated_bytes(),
            expected_allocation
        );
        reopened.revalidate().expect("revalidate reopened reserve");
        drop(reopened);

        let displaced = root.path().join("recovery-reserve.displaced");
        let replacement = b"replacement-must-survive";
        let replacement_failure =
            reopen_owned_reserve_with_test_hook(&root, &fixture.owner, &expected, || {
                fs::rename(&path, &displaced).expect("displace reserve child");
                fs::write(&path, replacement).expect("install replacement child");
            })
            .err()
            .expect("fixed-child replacement must be rejected");
        assert_eq!(
            replacement_failure.reason(),
            &ReserveNamespaceFailureReason::Observation(ReserveFailureReason::IdentityMismatch)
        );
        let retained = replacement_failure
            .into_file()
            .expect("replacement failure retains original descriptor");
        assert_eq!(
            stable_filesystem_identity_from_open_file(&retained).as_deref(),
            Some(expected_identity.as_str())
        );
        assert_eq!(
            retained.metadata().expect("retained metadata").len(),
            before_len
        );
        drop(retained);
        assert_eq!(fs::read(&path).expect("replacement bytes"), replacement);
        assert_eq!(
            fs::metadata(&displaced).expect("displaced metadata").len(),
            before_len
        );
        assert_eq!(allocation_bytes(&displaced), before_allocation);
        fs::remove_file(&path).expect("remove test replacement");
        fs::rename(&displaced, &path).expect("restore reserve child");

        let collision = create_owned_reserve(&root, &fixture.owner)
            .err()
            .expect("existing child must not be adopted");
        assert_eq!(collision.stage(), ReserveNamespaceFailureStage::Open);
        assert_eq!(
            collision.reason(),
            &ReserveNamespaceFailureReason::ChildExists
        );
        assert!(collision.into_file().is_none());
        assert_eq!(
            fs::metadata(&path).expect("collision metadata").len(),
            before_len
        );
        assert_eq!(allocation_bytes(&path), before_allocation);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn failed_namespace_sync_retains_entry_and_owned_descriptor() {
        let fixture = fixture();
        let root = root(&fixture, "reserve-root-sync-failure");
        let path = root.path().join(RESERVE_FILE_NAME);
        let failure = create_owned_reserve_with_test_sync(&root, &fixture.owner, |_, _| {
            Err("injected namespace sync failure".to_owned())
        })
        .err()
        .expect("injected namespace sync failure");
        assert_eq!(failure.stage(), ReserveNamespaceFailureStage::Sync);
        assert_eq!(
            failure.reason(),
            &ReserveNamespaceFailureReason::Sync("injected namespace sync failure".to_owned())
        );
        let observation = failure
            .observation()
            .expect("failed sync retains bounded observation")
            .clone();
        assert_eq!(observation.length(), PROTECTED_FREE_SPACE_FLOOR);
        let mut file = failure.into_file().expect("failed sync retains descriptor");
        file.write_all(b"sync-failure-marker")
            .expect("owner can recover retained file");
        file.sync_all().expect("recover retained file sync");
        assert!(path.exists());
        assert_eq!(
            fs::read(&path).expect("retained namespace entry bytes"),
            b"sync-failure-marker"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn symlink_and_missing_or_wrong_observations_fail_without_mutation() {
        use std::os::unix::fs::symlink;

        let fixture = fixture();
        let symlink_root = root(&fixture, "reserve-root-symlink");
        let target = symlink_root.path().join("target");
        fs::write(&target, b"target-marker").expect("symlink target");
        let child = symlink_root.path().join(RESERVE_FILE_NAME);
        symlink(&target, &child).expect("reserve symlink");
        let create = create_owned_reserve(&symlink_root, &fixture.owner)
            .err()
            .expect("symlink child must not be followed");
        assert_eq!(create.stage(), ReserveNamespaceFailureStage::Open);
        assert!(matches!(
            create.reason(),
            ReserveNamespaceFailureReason::ChildExists | ReserveNamespaceFailureReason::Open(_)
        ));
        assert_eq!(fs::read_link(&child).expect("unchanged symlink"), target);
        assert_eq!(
            fs::read(&target).expect("unchanged target"),
            b"target-marker"
        );

        let missing_root = root(&fixture, "reserve-root-missing");
        let missing_expected = synthetic_observation(missing_root.identity(), "missing-file");
        let missing = reopen_owned_reserve(&missing_root, &fixture.owner, &missing_expected)
            .err()
            .expect("missing child must require recovery");
        assert_eq!(
            missing.reason(),
            &ReserveNamespaceFailureReason::MissingChild
        );
        assert!(missing.into_file().is_none());

        let wrong_file_root = root(&fixture, "reserve-root-wrong-file");
        let wrong_file_path = wrong_file_root.path().join(RESERVE_FILE_NAME);
        fs::write(&wrong_file_path, b"wrong-file").expect("wrong reserve file");
        let wrong_file_expected = synthetic_observation(wrong_file_root.identity(), "other-file");
        let before = fs::read(&wrong_file_path).expect("wrong file bytes");
        let wrong_file =
            reopen_owned_reserve(&wrong_file_root, &fixture.owner, &wrong_file_expected)
                .err()
                .expect("wrong file identity must fail");
        assert!(matches!(
            wrong_file.reason(),
            ReserveNamespaceFailureReason::Observation(
                ReserveFailureReason::IdentityMismatch
                    | ReserveFailureReason::NotFresh { .. }
                    | ReserveFailureReason::LengthMismatch { .. }
                    | ReserveFailureReason::AllocationBelowFloor { .. }
                    | ReserveFailureReason::NotSingleLink { .. }
                    | ReserveFailureReason::NotRegular
                    | ReserveFailureReason::Compressed
                    | ReserveFailureReason::VerifiedVolumeMismatch
                    | ReserveFailureReason::VerifiedAllocationUnitMismatch
            )
        ));
        let wrong_file_handle = wrong_file.into_file().expect("retain wrong file handle");
        drop(wrong_file_handle);
        assert_eq!(
            fs::read(&wrong_file_path).expect("unchanged wrong file"),
            before
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn hardlink_and_wrong_root_are_rejected_without_replacement() {
        let fixture = fixture();
        let first_root = root(&fixture, "reserve-root-hardlink");
        let child = first_root.path().join(RESERVE_FILE_NAME);
        let wrong_root = root(&fixture, "reserve-root-other");
        let expected = synthetic_observation(first_root.identity(), "reserve-file");
        let wrong_root_failure = reopen_owned_reserve(&wrong_root, &fixture.owner, &expected)
            .err()
            .expect("root identity mismatch must fail before opening child");
        assert_eq!(
            wrong_root_failure.reason(),
            &ReserveNamespaceFailureReason::RootIdentityMismatch
        );

        let mut file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&child)
            .expect("hardlink source");
        file.write_all(b"hardlink-source")
            .expect("hardlink source marker");
        drop(file);
        let alias = first_root.path().join("reserve-alias");
        fs::hard_link(&child, &alias).expect("hardlink reserve child");
        let before = fs::read(&child).expect("hardlink bytes");
        let failure = create_owned_reserve(&first_root, &fixture.owner)
            .err()
            .expect("hardlink child must not be adopted");
        assert_eq!(
            failure.reason(),
            &ReserveNamespaceFailureReason::ChildExists
        );
        assert_eq!(fs::read(&child).expect("unchanged hardlink child"), before);
        assert_eq!(fs::read(&alias).expect("unchanged hardlink alias"), before);
        fs::remove_file(alias).expect("remove test hardlink alias");
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn unsupported_target_rejects_before_namespace_effects() {
        let fixture = fixture();
        let root = root(&fixture, "reserve-root-unsupported");
        let child = root.path().join(RESERVE_FILE_NAME);
        let failure = create_owned_reserve(&root, &fixture.owner)
            .err()
            .expect("unsupported target must fail closed");
        assert_eq!(failure.stage(), ReserveNamespaceFailureStage::Unsupported);
        assert_eq!(
            failure.reason(),
            &ReserveNamespaceFailureReason::Unsupported
        );
        assert!(!child.exists());
    }
}
