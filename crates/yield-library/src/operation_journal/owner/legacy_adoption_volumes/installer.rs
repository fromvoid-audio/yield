//! Original sealed volume/reserve installation and exact failure custody.
use super::*;

impl OwnerRuntime {
    pub(super) fn install_legacy_volume_custody(
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
            match super::super::super::capacity_volume_owner::acquire_legacy_for_descriptor(
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
}
