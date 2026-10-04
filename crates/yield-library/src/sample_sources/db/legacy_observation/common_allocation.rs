//! Descriptor-based common logical preparation. This does not create a physical capacity plan.
use super::{
    LegacyObservationError, LegacySourcePagerObservation, LegacySourceReadOwner, Result, refused,
};
use crate::{
    app_dirs::WritableProfileGuard,
    durable_capacity::VolumeIdentity,
    operation_journal::store::{LegacyJournalObservation, OperationJournalStore},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

/// Finite logical objects accounted before any admitted legacy effect.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum LegacyAllocationRole {
    /// Capped source main, including its retained original bytes.
    SourceMain,
    /// Proposed non-spilling source DELETE rollback.
    SourceRollback,
    /// Future separately admitted source WAL.
    SourceFutureWal,
    /// Future separately admitted source SHM.
    SourceFutureShm,
    /// Original fresh source reservation control descriptor.
    SourceReservationControl,
    /// Entire capped journal main, including indexes/intent/claim/receipt families.
    JournalMain,
    /// Proposed non-spilling journal DELETE rollback.
    JournalRollback,
    /// Original retained profile-owner control descriptor.
    ProfileOwnerControl,
}
/// One actual retained object or future direct-child namespace allowance, never a claim.
#[derive(Debug, Eq, PartialEq)]
pub struct LegacyAllocationObject {
    role: LegacyAllocationRole,
    identity: String,
    volume: VolumeIdentity,
    unit: u64,
    observed_bytes: u64,
    allowance: u64,
}
impl LegacyAllocationObject {
    /// Explicit purpose; no object is relabeled as an unrelated recovery payload.
    pub fn role(&self) -> LegacyAllocationRole {
        self.role
    }
    /// Actual file identity or original directory/direct-child identity for an absent sidecar.
    pub fn object_identity(&self) -> &str {
        &self.identity
    }
    /// Actual original file/directory descriptor volume; not free-capacity permission.
    pub fn volume(&self) -> &VolumeIdentity {
        &self.volume
    }
    /// Actual filesystem unit observation, not physical allocation qualification.
    pub fn allocation_unit(&self) -> u64 {
        self.unit
    }
    /// Actual retained length; zero for a positively absent future sidecar.
    pub fn observed_bytes(&self) -> u64 {
        self.observed_bytes
    }
    /// Conservative conditional logical allowance; positive even for absent future sidecars.
    pub fn proposed_logical_bytes(&self) -> u64 {
        self.allowance
    }
}
/// One deduplicated descriptor volume, summing each distinct logical object exactly once.
#[derive(Debug, Eq, PartialEq)]
pub struct LegacyAllocationVolume {
    volume: VolumeIdentity,
    unit: u64,
    logical_bytes: u64,
}
impl LegacyAllocationVolume {
    /// Actual descriptor-derived volume identity.
    pub fn volume(&self) -> &VolumeIdentity {
        &self.volume
    }
    /// Consistent observed filesystem unit for this volume.
    pub fn allocation_unit(&self) -> u64 {
        self.unit
    }
    /// Checked sum of conditional logical allowances, never allocated physical bytes.
    pub fn proposed_logical_bytes(&self) -> u64 {
        self.logical_bytes
    }
}
/// Private-construction common original-owner observation. No serde/claim/grant conversion.
#[derive(Debug, Eq, PartialEq)]
pub struct LegacyCommonAllocationObservation {
    source: LegacySourcePagerObservation,
    journal: LegacyJournalObservation,
    media_identity: String,
    source_directory_identity: String,
    objects: [LegacyAllocationObject; 8],
    volumes: Vec<LegacyAllocationVolume>,
}
impl LegacyCommonAllocationObservation {
    /// Original exact bounded source image and proposed pager policy.
    pub fn source(&self) -> &LegacySourcePagerObservation {
        &self.source
    }
    /// Actual original journal/profile facts and proposed journal policy.
    pub fn journal(&self) -> &LegacyJournalObservation {
        &self.journal
    }
    /// Original media-root descriptor identity; no filesystem audit is manufactured.
    pub fn media_root_identity(&self) -> &str {
        &self.media_identity
    }
    /// Original source database directory, used for absent sidecar placement.
    pub fn source_directory_identity(&self) -> &str {
        &self.source_directory_identity
    }
    /// Eight finite distinct object allowances, in fixed role order.
    pub fn objects(&self) -> &[LegacyAllocationObject; 8] {
        &self.objects
    }
    /// Distinct volumes in stable descriptor-device order, with no alias duplication.
    pub fn volumes(&self) -> &[LegacyAllocationVolume] {
        &self.volumes
    }
}
impl LegacySourceReadOwner {
    /// Compose actual original source and journal/profile custody within one cooperative two-
    /// second deadline. Capture enters no page cap/persistent policy and writes no intent/claim.
    /// Native physical admission, directory metadata qualification and the explicit adoption
    /// command/receipt remain separate. Returned facts are historical, not live authority.
    pub fn capture_common_allocation_observation(
        &self,
        profile: &WritableProfileGuard,
        journal: &OperationJournalStore,
    ) -> Result<LegacyCommonAllocationObservation> {
        self.capture_common_until(profile, journal, Instant::now() + Duration::from_secs(2))
    }
    pub(super) fn capture_common_until(
        &self,
        profile: &WritableProfileGuard,
        journal: &OperationJournalStore,
        deadline: Instant,
    ) -> Result<LegacyCommonAllocationObservation> {
        let _extensions = crate::sqlite_ext::try_extension_free_metadata_scope()
            .ok_or(LegacyObservationError("SQLite extension runtime unknown"))?;
        if Instant::now() >= deadline {
            return Err(LegacyObservationError(
                "common observation deadline elapsed",
            ));
        }
        let source = self.capture_pager_until(profile, deadline)?;
        let journal_image = journal
            .capture_legacy_journal_until(profile, deadline)
            .map_err(refused)?;
        let directory = self
            .binding
            .root()
            .try_clone_descriptor()
            .map_err(refused)?;
        let directory_facts =
            crate::durable_capacity::descriptor_capacity_facts(&directory).map_err(refused)?;
        let (control_bytes, control_facts) = self
            .reservation
            .observation_lock_facts(profile, &self.binding)
            .map_err(refused)?;
        if control_bytes != 0 {
            return Err(LegacyObservationError(
                "initial reservation control is not empty",
            ));
        }
        use LegacyAllocationRole::*;
        let root = self.binding.root().identity();
        let future = |name: String| format!("directory:{root}/{name}");
        let journal_rollback_identity = journal_image
            .existing_rollback_identity()
            .map(str::to_owned)
            .unwrap_or_else(|| {
                format!(
                    "directory:{}/{}-journal",
                    journal_image.directory_identity(),
                    crate::operation_journal::store::JOURNAL_DATABASE_FILE_NAME
                )
            });
        let object =
            |role, identity: String, volume: &VolumeIdentity, unit, observed_bytes, allowance| {
                LegacyAllocationObject {
                    role,
                    identity,
                    volume: volume.clone(),
                    unit,
                    observed_bytes,
                    allowance,
                }
            };
        let objects = [
            object(
                SourceMain,
                self.binding.identity().into(),
                source.volume(),
                source.allocation_unit(),
                source.image().main_bytes(),
                source.database_bytes(),
            ),
            object(
                SourceRollback,
                future(format!("{}-journal", super::super::DB_FILE_NAME)),
                &directory_facts.identity,
                directory_facts.allocation_unit,
                0,
                source.rollback_bytes(),
            ),
            object(
                SourceFutureWal,
                future(format!("{}-wal", super::super::DB_FILE_NAME)),
                &directory_facts.identity,
                directory_facts.allocation_unit,
                0,
                source.future_wal_bytes(),
            ),
            object(
                SourceFutureShm,
                future(format!("{}-shm", super::super::DB_FILE_NAME)),
                &directory_facts.identity,
                directory_facts.allocation_unit,
                0,
                source.future_shm_bytes(),
            ),
            object(
                SourceReservationControl,
                self.reservation.observation_lock_identity().into(),
                &control_facts.identity,
                control_facts.allocation_unit,
                control_bytes,
                4096,
            ),
            object(
                JournalMain,
                journal_image.database_identity().into(),
                journal_image.volume(),
                journal_image.allocation_unit(),
                journal_image.main_bytes(),
                journal_image.proposed_database_bytes(),
            ),
            object(
                JournalRollback,
                journal_rollback_identity,
                journal_image.sidecar_volume(),
                journal_image.sidecar_allocation_unit(),
                journal_image.existing_rollback_bytes(),
                journal_image.proposed_rollback_bytes(),
            ),
            object(
                ProfileOwnerControl,
                journal_image.profile_lock_identity().into(),
                journal_image.profile_lock_volume(),
                journal_image.profile_lock_allocation_unit(),
                journal_image.profile_lock_bytes(),
                4096,
            ),
        ];
        let volumes = aggregate(&objects)?;
        // Revalidate the original frozen source and journal facts, rather than return a proposed
        // allowance after an external writer/replacement/policy change or a renewed deadline.
        if self.capture_pager_until(profile, deadline)? != source
            || journal
                .capture_legacy_journal_until(profile, deadline)
                .map_err(refused)?
                != journal_image
            || Instant::now() >= deadline
        {
            return Err(LegacyObservationError(
                "common original facts changed or deadline elapsed",
            ));
        }
        let (final_control_bytes, final_control) = self
            .reservation
            .observation_lock_facts(profile, &self.binding)
            .map_err(refused)?;
        if final_control_bytes != control_bytes
            || final_control.identity != control_facts.identity
            || final_control.allocation_unit != control_facts.allocation_unit
            || Instant::now() >= deadline
        {
            return Err(LegacyObservationError(
                "source control facts changed or deadline elapsed",
            ));
        }
        Ok(LegacyCommonAllocationObservation {
            source,
            journal: journal_image,
            media_identity: self.media_root.identity().into(),
            source_directory_identity: root.into(),
            objects,
            volumes,
        })
    }
}
fn aggregate(objects: &[LegacyAllocationObject; 8]) -> Result<Vec<LegacyAllocationVolume>> {
    let mut roles = BTreeSet::new();
    let mut identities = BTreeSet::new();
    let mut volumes: BTreeMap<VolumeIdentity, (u64, u64)> = BTreeMap::new();
    for object in objects {
        if !roles.insert(object.role)
            || !identities.insert(&object.identity)
            || object.identity.is_empty()
            || object.unit == 0
            || !object.unit.is_power_of_two()
            || object.allowance == 0
            || object.observed_bytes > object.allowance
        {
            return Err(LegacyObservationError(
                "ambiguous object, unit or allowance",
            ));
        }
        let entry = volumes
            .entry(object.volume.clone())
            .or_insert((object.unit, 0));
        if entry.0 != object.unit {
            return Err(LegacyObservationError(
                "conflicting volume allocation units",
            ));
        }
        entry.1 = entry
            .1
            .checked_add(object.allowance)
            .ok_or(LegacyObservationError("common allowance overflow"))?;
    }
    Ok(volumes
        .into_iter()
        .map(|(volume, (unit, logical_bytes))| LegacyAllocationVolume {
            volume,
            unit,
            logical_bytes,
        })
        .collect())
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[cfg(test)]
mod tests;
