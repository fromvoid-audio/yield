//! Actual pre-lease reserve custody on the original worker; no ordinary admission permission.
use super::legacy_adoption_custody::RetainedLegacyAdoption;
use super::*;
use crate::sample_sources::db::{BoundDatabaseRoot, SourceCommandId};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

const PATH_BACKING: usize = 8192;
const IDENTITY_BACKING: usize = 256;
pub(super) const REPLY_BYTES: usize = std::mem::size_of::<
    super::super::legacy_adoption_commands::LegacyAdoptionIntentArgs,
>() + 13 * 1024;
const ROOT_BYTES: usize = std::mem::size_of::<BoundDatabaseRoot>()
    + 2 * PATH_BACKING
    + 2 * IDENTITY_BACKING
    + 2 * std::mem::size_of::<usize>()
    + std::mem::size_of::<std::fs::File>();
const OWNER_BYTES: usize = std::mem::size_of::<VolumeOwnershipCapability>()
    + 2 * PATH_BACKING
    + 3 * IDENTITY_BACKING
    + 6 * std::mem::size_of::<usize>();
const RESERVE_BYTES: usize = std::mem::size_of::<OwnedReserveAllocation>()
    + 2 * PATH_BACKING
    + 5 * IDENTITY_BACKING
    + 2 * std::mem::size_of::<usize>()
    + std::mem::size_of::<std::fs::File>();
const FAILURE_BYTES: usize = std::mem::size_of::<
    super::super::capacity_volume_owner::LegacyVolumeFailureBacking,
>() + OWNER_BYTES
    + PATH_BACKING
    + 2 * IDENTITY_BACKING
    + std::mem::size_of::<crate::app_dirs::LegacyAppRootBacking>()
    + std::mem::size_of::<super::super::capacity_reserve_namespace::LegacyReserveFailureBacking>()
    + 2 * IDENTITY_BACKING;
// Checkpoint/current/final copies + conservative serde JSON scratch/output overlap, two
// original-root/owner/reserve/failure inventories and one temporary context alias. The original
// source/native SQLite allocator and physical extents are outside this retained metadata bound.
const RETAINED_BYTES: usize = std::mem::size_of::<RetainedLegacyVolumes>()
    + 3 * REPLY_BYTES
    + 3 * super::super::legacy_adoption_commands::MAX_LEGACY_ADOPTION_INPUT_BYTES
    + 2 * (std::mem::size_of::<LegacyVolumeEntry>()
        + ROOT_BYTES
        + OWNER_BYTES
        + RESERVE_BYTES
        + FAILURE_BYTES)
    + OWNER_BYTES
    + 4 * PATH_BACKING // Simultaneous canonical home/library/support/app paths.
    + std::mem::size_of::<crate::sample_sources::db::LegacyReserveUseProof<'static>>()
    + 2 * (std::mem::size_of::<Vec<crate::sample_sources::db::LegacyReserveTarget>>()
        + 4 * std::mem::size_of::<crate::sample_sources::db::LegacyReserveTarget>())
    + 1024;

pub(super) struct RetainedLegacyVolumes {
    // Release reserve capsules before volume locks, and backing before its metadata credit.
    pub(super) entries: Vec<LegacyVolumeEntry>,
    original_digest: [u8; 32],
    checkpoint: super::super::legacy_adoption_commands::LegacyAdoptionIntentArgs,
    pub(super) ready: bool,
    _input: InputPermit,
}
pub(super) enum LegacyVolumeEntry {
    Existing {
        volume: VolumeIdentity,
        unit: u64,
        provenance: LiveDescriptorCapacityProvenance,
    },
    Owned {
        acquisition_failure:
            Option<super::super::capacity_volume_owner::LegacyVolumeFailureBacking>,
        namespace_failure:
            Option<super::super::capacity_reserve_namespace::LegacyReserveFailureBacking>,
        reserve: Option<OwnedReserveAllocation>,
        owner: Option<VolumeOwnershipCapability>,
        root: Arc<BoundDatabaseRoot>,
        volume: VolumeIdentity,
        unit: u64,
    },
}
impl RetainedLegacyVolumes {
    fn metadata_bytes(&self) -> Option<usize> {
        let mut bytes = std::mem::size_of::<Self>()
            + self.entries.capacity() * std::mem::size_of::<LegacyVolumeEntry>()
            + self.checkpoint.legacy_metadata_bytes()?;
        for entry in &self.entries {
            if let LegacyVolumeEntry::Existing { provenance, .. } = entry {
                bytes += provenance.profile_identity.capacity()
                    + provenance.reserve_root_identity.capacity()
                    + provenance.reserve_file_identity.capacity();
            }
            if let LegacyVolumeEntry::Owned {
                root,
                owner,
                reserve,
                namespace_failure,
                acquisition_failure,
                ..
            } = entry
            {
                bytes += root.legacy_metadata_bytes()?;
                if let Some(owner) = owner {
                    bytes += owner.legacy_metadata_bytes()?;
                }
                if let Some(reserve) = reserve {
                    bytes += reserve.legacy_metadata_bytes()?;
                }
                if let Some(failure) = namespace_failure {
                    bytes += failure.metadata_bytes();
                }
                if let Some(failure) = acquisition_failure {
                    bytes += failure.metadata_bytes();
                }
            }
        }
        Some(bytes)
    }
}
impl OwnerRuntime {
    pub(super) fn prepare_legacy_volumes(
        &mut self,
        command: SourceCommandId,
        digest: [u8; 32],
        input: InputPermit,
    ) -> Result<LegacyAdoptionCustody, OperationJournalOwnerError> {
        use LegacyAdoptionCustodyError as E;
        let retained = self.legacy_adoption.as_ref().ok_or(E::Closed)?;
        if retained.args.source_command_id() != command
            || (retained.digest != digest
                && !retained
                    .volumes
                    .as_ref()
                    .is_some_and(|v| v.original_digest == digest))
        {
            return Err(E::Changed.into());
        }
        if let Some(volumes) = &retained.volumes {
            if volumes
                .metadata_bytes()
                .is_none_or(|b| b > volumes._input.bytes)
            {
                return Err(E::ReserveRecovery.into());
            }
            if !volumes.ready {
                return Err(E::ReserveRecovery.into());
            }
            self.validate_legacy_custody(command, retained.digest)?;
            self.validate_legacy_volume_bindings()?;
            return Ok(LegacyAdoptionCustody {
                _reply_input: Some(input),
                args: retained.args.clone(),
                digest: retained.digest,
                owner: Arc::downgrade(&retained.input.shared),
            });
        }
        self.validate_legacy_custody(command, digest)?;
        let mut retained = self.legacy_adoption.take().ok_or(E::Closed)?;
        let result = self.install_legacy_volume_custody(&mut retained, input);
        // Every ordinary refusal retains the same original source and any partial backing.
        self.legacy_adoption = Some(retained);
        result
    }
    fn install_legacy_volume_custody(
        &mut self,
        retained: &mut RetainedLegacyAdoption,
        input: InputPermit,
    ) -> Result<LegacyAdoptionCustody, OperationJournalOwnerError> {
        use LegacyAdoptionCustodyError as E;
        if retained.args.legacy_metadata_bytes().is_none() || input.bytes != REPLY_BYTES {
            return Err(E::Changed.into());
        }
        // Each tree node holds at most eleven entries and twelve child pointers in the pinned
        // Rust implementation. Charge a full twelve-entry node per observed volume, including
        // bounded provenance strings, rather than relying on sharing/dense occupancy.
        let node = 12
            * (std::mem::size_of::<(VolumeIdentity, VolumeFacts)>()
                + std::mem::size_of::<(VolumeIdentity, LiveDescriptorCapacityProvenance)>())
            + 24 * std::mem::size_of::<usize>()
            + 2 * 32
            + 3 * IDENTITY_BACKING;
        let map_bytes = self
            .source_volumes
            .len()
            .checked_add(3)
            .and_then(|n| n.checked_mul(node))
            .and_then(|n| n.checked_add(RETAINED_BYTES))
            .ok_or(E::Changed)?;
        let metadata = input
            .shared
            .reserve_input(InputClass::Ordinary, map_bytes)
            .map_err(|_| E::Changed)?;
        let deadline = Instant::now() + Duration::from_secs(2);
        let profile = self
            .final_profile_guard
            .as_ref()
            .ok_or(E::Closed)?
            .try_clone()
            .map_err(|_| E::Changed)?;
        if input.shared.status() != OperationJournalOwnerStatus::Available {
            return Err(E::Closed.into());
        }
        self.store()
            .validate_for_admission()
            .map_err(|_| E::Changed)?;
        let targets = retained
            .source
            .reserve_targets(&profile, deadline)
            .map_err(|_| E::Changed)?;
        if targets
            .iter()
            .any(|t| t.root.legacy_metadata_bytes().is_none())
        {
            return Err(E::Changed.into());
        }
        if targets.capacity() > 4 {
            return Err(E::Changed.into());
        }
        retained
            .source
            .reserve_use_proof(&profile, deadline)
            .map_err(|_| E::Changed)?;
        // Existing physical bootstrap gates stay closed unless the original profile reserve and
        // every installed source reserve can currently prove the complete protected floor.
        #[cfg(all(test, target_os = "macos"))]
        let observation = self.refresh_live_descriptor_capacity_for_admission(None);
        #[cfg(not(all(test, target_os = "macos")))]
        let observation = self.refresh_live_descriptor_capacity_for_admission();
        let observation = observation.map_err(|_| E::Changed)?;
        let mut entries = Vec::with_capacity(2);
        for target in targets {
            if let Some(facts) = observation.facts.get(&target.volume) {
                if facts.allocation_unit != target.unit {
                    return Err(E::Changed.into());
                }
                entries.push(LegacyVolumeEntry::Existing {
                    volume: target.volume.clone(),
                    unit: target.unit,
                    provenance: {
                        let origin = observation
                            .provenance
                            .get(&target.volume)
                            .ok_or(E::Changed)?;
                        if [
                            &origin.profile_identity,
                            &origin.reserve_root_identity,
                            &origin.reserve_file_identity,
                        ]
                        .iter()
                        .any(|s| s.capacity() > IDENTITY_BACKING)
                        {
                            return Err(E::Changed.into());
                        }
                        origin.clone()
                    },
                });
            } else {
                entries.push(LegacyVolumeEntry::Owned {
                    acquisition_failure: None,
                    namespace_failure: None,
                    reserve: None,
                    owner: None,
                    root: target.root,
                    volume: target.volume,
                    unit: target.unit,
                });
            }
        }
        retained
            .source
            .reserve_use_proof(&profile, deadline)
            .map_err(|_| E::Changed)?;
        retained.volumes = Some(RetainedLegacyVolumes {
            entries,
            original_digest: retained.digest,
            checkpoint: retained.args.clone(),
            ready: false,
            _input: metadata,
        });
        for index in 0..retained.volumes.as_ref().unwrap().entries.len() {
            self.verify_legacy_prefix(retained, &profile, deadline)?;
            let proof = retained
                .source
                .reserve_use_proof(&profile, deadline)
                .map_err(|_| E::ReserveRecovery)?;
            let entry = &mut retained.volumes.as_mut().unwrap().entries[index];
            let LegacyVolumeEntry::Owned {
                root,
                owner,
                reserve,
                volume,
                unit,
                namespace_failure,
                acquisition_failure,
            } = entry
            else {
                continue;
            };
            proof
                .validate_for_bootstrap()
                .map_err(|_| E::ReserveRecovery)?;
            let descriptor = root
                .try_clone_descriptor()
                .map_err(|_| E::ReserveRecovery)?;
            let facts = descriptor_capacity_facts(&descriptor).map_err(|_| E::ReserveRecovery)?;
            if facts.identity != *volume || facts.allocation_unit != *unit {
                return Err(E::ReserveRecovery.into());
            }
            match super::super::capacity_volume_owner::acquire_legacy_for_descriptor(
                &descriptor,
                &profile,
                &proof,
            ) {
                Ok(acquired) => *owner = Some(acquired),
                Err(backing) => {
                    *acquisition_failure = Some(backing);
                    return Err(E::ReserveRecovery.into());
                }
            }
            proof
                .validate_for_bootstrap()
                .map_err(|_| E::ReserveRecovery)?;
            let context_owner = owner
                .as_ref()
                .unwrap()
                .try_clone_for_context()
                .map_err(|_| E::ReserveRecovery)?;
            match self
                .store()
                .acquire_owned_legacy_reserve(context_owner, root, &profile, &proof)
            {
                Ok(acquired) => *reserve = Some(acquired),
                Err(failure) => {
                    *owner = Some(failure.original.volume_owner);
                    *reserve = failure.original.reserve.or(failure.reopened_reserve);
                    *namespace_failure = failure.namespace;
                    return Err(E::ReserveRecovery.into());
                }
            }
            self.advance_legacy_prefix(retained, &profile, deadline)?;
        }
        self.verify_legacy_prefix(retained, &profile, deadline)?;
        self.store()
            .validate_for_admission()
            .map_err(|_| E::ReserveRecovery)?;
        let args = retained
            .source
            .prepare_legacy_adoption_input_until(
                &profile,
                self.store(),
                retained.args.participant_id(),
                retained.args.source_command_id(),
                &AtomicBool::new(false),
                deadline,
            )
            .map_err(|_| E::ReserveRecovery)?;
        if args != retained.volumes.as_ref().unwrap().checkpoint
            || args.original_image_digest() != retained.args.original_image_digest()
            || args.source_database_identity() != retained.args.source_database_identity()
            || args.source_reservation_identity() != retained.args.source_reservation_identity()
            || args.profile_owner_identity() != retained.args.profile_owner_identity()
            || input_status(retained) != OperationJournalOwnerStatus::Available
        {
            return Err(E::ReserveRecovery.into());
        }
        let encoded = args.canonical_bytes().map_err(|_| E::ReserveRecovery)?;
        if args.legacy_metadata_bytes().is_none() {
            return Err(E::ReserveRecovery.into());
        }
        let digest = *blake3::hash(&encoded).as_bytes();
        retained.args = args;
        retained.digest = digest;
        retained.volumes.as_mut().unwrap().ready = true;
        Ok(LegacyAdoptionCustody {
            _reply_input: Some(input),
            args: retained.args.clone(),
            digest,
            owner: Arc::downgrade(&retained.input.shared),
        })
    }
    fn current_legacy_prefix(
        &self,
        retained: &RetainedLegacyAdoption,
        profile: &WritableProfileGuard,
        deadline: Instant,
    ) -> Result<
        super::super::legacy_adoption_commands::LegacyAdoptionIntentArgs,
        OperationJournalOwnerError,
    > {
        let args = retained
            .source
            .prepare_legacy_adoption_input_until(
                profile,
                self.store(),
                retained.args.participant_id(),
                retained.args.source_command_id(),
                &AtomicBool::new(false),
                deadline,
            )
            .map_err(|_| LegacyAdoptionCustodyError::ReserveRecovery)?;
        if args.original_image_digest() != retained.args.original_image_digest()
            || args.source_database_identity() != retained.args.source_database_identity()
            || args.source_reservation_identity() != retained.args.source_reservation_identity()
            || args.profile_owner_identity() != retained.args.profile_owner_identity()
        {
            return Err(LegacyAdoptionCustodyError::ReserveRecovery.into());
        }
        Ok(args)
    }
    fn verify_legacy_prefix(
        &self,
        retained: &RetainedLegacyAdoption,
        profile: &WritableProfileGuard,
        deadline: Instant,
    ) -> Result<(), OperationJournalOwnerError> {
        let current = self.current_legacy_prefix(retained, profile, deadline)?;
        if current
            != retained
                .volumes
                .as_ref()
                .ok_or(LegacyAdoptionCustodyError::Closed)?
                .checkpoint
        {
            return Err(LegacyAdoptionCustodyError::ReserveRecovery.into());
        }
        Ok(())
    }
    fn advance_legacy_prefix(
        &self,
        retained: &mut RetainedLegacyAdoption,
        profile: &WritableProfileGuard,
        deadline: Instant,
    ) -> Result<(), OperationJournalOwnerError> {
        let current = self.current_legacy_prefix(retained, profile, deadline)?;
        retained
            .volumes
            .as_mut()
            .ok_or(LegacyAdoptionCustodyError::Closed)?
            .checkpoint = current;
        Ok(())
    }
    pub(super) fn observe_legacy_reserve_facts(
        &mut self,
        facts: &mut BTreeMap<VolumeIdentity, VolumeFacts>,
        provenance: &mut BTreeMap<VolumeIdentity, LiveDescriptorCapacityProvenance>,
        full_floor: bool,
        #[cfg(test)] reader: Option<
            &dyn Fn(
                &mut OwnedReserveAllocation,
                &WritableProfileGuard,
            ) -> Result<VolumeFacts, String>,
        >,
    ) -> Result<(), String> {
        self.validate_legacy_volume_bindings()
            .map_err(|e| e.to_string())?;
        let Some(volumes) = self
            .legacy_adoption
            .as_mut()
            .and_then(|s| s.volumes.as_mut())
        else {
            return Ok(());
        };
        let profile = self
            .final_profile_guard
            .as_ref()
            .ok_or("legacy profile owner is unavailable")?;
        for entry in &mut volumes.entries {
            match entry {
                LegacyVolumeEntry::Existing {
                    volume,
                    unit,
                    provenance: expected,
                } => {
                    if facts.get(volume).is_none_or(|f| f.allocation_unit != *unit) {
                        return Err(
                            "legacy original volume no longer has its qualified binding".into()
                        );
                    }
                    if provenance.get(volume) != Some(expected) {
                        return Err("legacy original qualified provenance changed".into());
                    }
                }
                LegacyVolumeEntry::Owned {
                    owner,
                    reserve,
                    volume,
                    unit,
                    ..
                } => {
                    if facts.contains_key(volume) {
                        return Err(
                            "legacy volume ownership overlaps another original owner".into()
                        );
                    }
                    let owner = owner.as_ref().ok_or("legacy owner is incomplete")?;
                    let reserve = reserve.as_mut().ok_or("legacy reserve is incomplete")?;
                    let (observed, current, origin) = observe_retained_descriptor_inner(
                        owner,
                        reserve,
                        profile,
                        full_floor,
                        #[cfg(test)]
                        reader,
                    )?;
                    if observed != *volume || current.allocation_unit != *unit {
                        return Err("legacy original reserve volume or unit changed".into());
                    }
                    facts.insert(observed.clone(), current);
                    provenance.insert(observed, origin);
                }
            }
        }
        Ok(())
    }
    pub(super) fn validate_legacy_volume_bindings(&self) -> Result<(), OperationJournalOwnerError> {
        use LegacyAdoptionCustodyError as E;
        let Some(volumes) = self
            .legacy_adoption
            .as_ref()
            .and_then(|s| s.volumes.as_ref())
        else {
            return Ok(());
        };
        if !volumes.ready {
            return Err(E::ReserveRecovery.into());
        }
        let profile = self.final_profile_guard.as_ref().ok_or(E::Closed)?;
        let retained = self.legacy_adoption.as_ref().unwrap();
        if retained
            .source
            .capture(profile)
            .map_err(|_| E::Changed)?
            .digest()
            != retained.args.original_image_digest()
        {
            return Err(E::Changed.into());
        }
        self.legacy_adoption
            .as_ref()
            .unwrap()
            .source
            .reserve_use_proof(profile, Instant::now() + Duration::from_secs(2))
            .map_err(|_| E::Changed)?;
        for entry in &volumes.entries {
            match entry {
                LegacyVolumeEntry::Existing {
                    volume,
                    unit,
                    provenance,
                } => {
                    if self
                        .volume_owner
                        .as_ref()
                        .is_some_and(|o| o.volume() == volume)
                    {
                        let reserve = self.reserve.as_ref().ok_or(E::Changed)?;
                        if reserve.observation().allocation_unit() != *unit {
                            return Err(E::Changed.into());
                        }
                        validate_existing_provenance(
                            self.volume_owner.as_ref().unwrap(),
                            reserve,
                            provenance,
                        )?;
                    } else {
                        let binding = self.source_volumes.get(volume).ok_or(E::Changed)?;
                        if binding.readiness != SourceVolumeBindingReadiness::Complete
                            || binding.allocation_unit != Some(*unit)
                        {
                            return Err(E::Changed.into());
                        }
                        validate_existing_provenance(
                            binding.owner.as_ref().ok_or(E::Changed)?,
                            binding.reserve.as_ref().ok_or(E::Changed)?,
                            provenance,
                        )?;
                    }
                }
                LegacyVolumeEntry::Owned {
                    root,
                    volume,
                    unit,
                    owner,
                    reserve,
                    ..
                } => {
                    root.validate_profile_guard(profile)
                        .map_err(|_| E::Changed)?;
                    let owner = owner.as_ref().ok_or(E::ReserveRecovery)?;
                    owner.revalidate().map_err(|_| E::Changed)?;
                    let reserve = reserve.as_ref().ok_or(E::ReserveRecovery)?;
                    if owner.volume() != volume
                        || reserve.observation().volume() != volume
                        || reserve.observation().allocation_unit() != *unit
                    {
                        return Err(E::Changed.into());
                    }
                }
            }
        }
        Ok(())
    }
}
fn input_status(retained: &RetainedLegacyAdoption) -> OperationJournalOwnerStatus {
    retained.input.shared.status()
}

fn validate_existing_provenance(
    owner: &VolumeOwnershipCapability,
    reserve: &OwnedReserveAllocation,
    expected: &LiveDescriptorCapacityProvenance,
) -> Result<(), OperationJournalOwnerError> {
    let observation = reserve.observation();
    if owner.owner_generation() != expected.owner_generation
        || owner.owner_profile_identity() != expected.profile_identity
        || observation.root_identity() != expected.reserve_root_identity
        || observation.file_identity() != expected.reserve_file_identity
        || observation.allocation_unit() != expected.allocation_unit
    {
        return Err(LegacyAdoptionCustodyError::Changed.into());
    }
    owner
        .revalidate()
        .map_err(|_| LegacyAdoptionCustodyError::Changed.into())
}

#[cfg(test)]
mod tests;
