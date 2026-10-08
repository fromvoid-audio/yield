//! Original worker consumer. Passive V19 bytes never reconstruct this custody.
use super::*;
use crate::operation_journal::{
    admission_claims::AdmissionClaim, capacity_provider::ProvisionalCapacityLease,
    store::BorrowedInitialClaimSettlementError,
};
use crate::sample_sources::db::SourceCommandId;
use std::time::Instant;

// Original capture scratch plus closed eight-role validation/claim metadata.
// Variable command strings and serialization graphs receive a measured loan below.
pub(super) const INPUT_BYTES: usize = legacy_adoption_custody::INPUT_BYTES
    + std::mem::size_of::<Retained>()
    + 8 * (12 * std::mem::size_of::<(u64, (u64, u64, u64))>()
        + 16 * std::mem::size_of::<usize>()
        + 2 * std::mem::size_of::<AdmissionClaim>());

pub(super) struct Retained {
    record: StoredOperationRecord,
    encoded: Vec<u8>,
    claims: AdmissionClaimInventory,
    lease: ProvisionalCapacityLease,
    outcome: Option<JournalInsertOutcome>,
    failure: Option<BorrowedInitialClaimSettlementError>,
    // Frozen data and selected outcomes die before their original backing credit.
    _construction: InputPermit,
    _encoding: InputPermit,
    _input: InputPermit,
}

impl OwnerRuntime {
    pub(super) fn admit_legacy_initial(
        &mut self,
        command: SourceCommandId,
        digest: [u8; 32],
        deadline: Instant,
        input: InputPermit,
    ) -> Result<OperationId, OperationJournalOwnerError> {
        use LegacyAdoptionCustodyError as E;
        let retained = self.legacy_adoption.as_ref().ok_or(E::Closed)?;
        if Instant::now() >= deadline
            || retained.input.shared.status() != OperationJournalOwnerStatus::Available
        {
            return Err(E::Closed.into());
        }
        if retained.args.source_command_id() != command || retained.digest != digest {
            return Err(E::Changed.into());
        }
        let profile = self.final_profile_guard.as_ref().ok_or(E::Closed)?;
        if let Some(admission) = &retained.admission {
            if admission.failure.is_some()
                || !matches!(admission.outcome, Some(JournalInsertOutcome::Committed(_)))
            {
                return Err(E::ReserveRecovery.into());
            }
            retained
                .source
                .validate_admission_checkpoint_until(profile, &retained.args, deadline)
                .map_err(|_| E::Changed)?;
            input.fund_historical_consumer()?;
            let snapshot = self.store().rebuild_admission_snapshot_for_input(&input)?;
            if !matches!(snapshot.readiness(), CapacityRebuildReadiness::Ready) {
                return Err(E::ReserveRecovery.into());
            }
            let provider = self.capacity_provider.as_ref().ok_or(E::Closed)?;
            snapshot
                .validate_provider(provider, Some(&input))
                .map_err(|_| E::ReserveRecovery)?;
            provider
                .validate_recovery_state()
                .map_err(|_| E::ReserveRecovery)?;
            if admission.claims.claims().iter().any(|claim| {
                !snapshot.claims().iter().any(|row| {
                    row.claim_id() == claim.claim_id()
                        && row.operation_id() == admission.record.operation_id
                        && row.volume() == claim.volume()
                        && row.allocation_class() == claim.allocation_class()
                        && row.allocated_bytes() == claim.allocated_bytes()
                        && row.release_id().is_none()
                        && !row.audit_pending()
                })
            }) {
                return Err(E::ReserveRecovery.into());
            }
            self.store().verify_legacy_initial_frozen(
                &admission.record,
                &admission.claims,
                &input,
            )?;
            if Instant::now() >= deadline {
                return Err(E::Changed.into());
            }
            return Ok(admission.record.operation_id);
        }
        if !retained.volumes.as_ref().is_some_and(|v| v.ready) {
            return Err(E::ReserveRecovery.into());
        }
        self.verify_legacy_prefix(retained, profile, deadline)?;
        retained
            .source
            .validate_admission_checkpoint_until(profile, &retained.args, deadline)
            .map_err(|_| E::Changed)?;
        self.store()
            .validate_namespace_with_input(Some(&input), false)?;
        input.fund_historical_consumer()?;
        let snapshot = self.store().rebuild_admission_snapshot_for_input(&input)?;
        match snapshot.readiness() {
            CapacityRebuildReadiness::Ready => {}
            CapacityRebuildReadiness::RecoveryOnly { .. } if snapshot.proves_durable_zero() => {}
            _ => return Err(E::ReserveRecovery.into()),
        }
        let construction =
            OperationJournalStore::prepay_initial_record_construction(&retained.args, &input)?;
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|v| i64::try_from(v.as_millis()).ok())
            .ok_or(E::Changed)?;
        let record = StoredOperationRecord::new_v19_legacy_adoption(
            OperationId::new(),
            retained.args.clone(),
            timestamp,
        )
        .map_err(E::Preparation)?;
        let plan = record.full_capacity_plan.as_ref().ok_or(E::Changed)?;
        let claims = AdmissionClaimInventory::new(
            plan.allocations()
                .iter()
                .map(|row| {
                    AdmissionClaim::new(
                        AdmissionClaimId::new(),
                        row.volume().clone(),
                        row.class(),
                        row.allocated_bytes(),
                    )
                })
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| E::Changed)?,
        )
        .map_err(|_| E::Changed)?;
        let encoding = OperationJournalStore::prepay_initial_frozen_encoding(&record, &input)?;
        let encoded = encode(&record).map_err(|_| E::Changed)?;
        let observation_credit = self.admission_observation_credit(&input)?;
        #[cfg(all(test, target_os = "macos"))]
        let observation = self.refresh_live_descriptor_capacity_for_admission(None);
        #[cfg(not(all(test, target_os = "macos")))]
        let observation = self.refresh_live_descriptor_capacity_for_admission();
        let observation = observation.map_err(|_| E::Changed)?;
        let witness = self
            .live_admission_witness(observation)
            .map_err(|_| E::Changed)?;
        let provider = self.capacity_provider.as_ref().ok_or(E::Closed)?;
        snapshot
            .validate_provider(provider, Some(&input))
            .map_err(|_| E::ReserveRecovery)?;
        provider
            .validate_recovery_state()
            .map_err(|_| E::ReserveRecovery)?;
        provider
            .validate_claim_identities_available(&claims)
            .map_err(|_| E::Changed)?;
        let lease = provider
            .admit_with_live_witness_for_input(plan, &claims, witness, &input)
            .map_err(|_| E::Changed)?;
        drop(observation_credit);
        // The original live witness has been consumed. Freeze the exact lease and
        // intent in the sole source slot before any pin or transaction can run.
        self.legacy_adoption.as_mut().ok_or(E::Closed)?.admission = Some(Retained {
            record,
            encoded,
            claims,
            lease,
            outcome: None,
            failure: None,
            _construction: construction,
            _encoding: encoding,
            _input: input,
        });
        let retained = self.legacy_adoption.as_mut().ok_or(E::Closed)?;
        let admission = retained.admission.as_mut().ok_or(E::Closed)?;
        let profile = self.final_profile_guard.as_ref().ok_or(E::Closed)?;
        if retained
            .source
            .validate_admission_checkpoint_until(profile, &retained.args, deadline)
            .is_err()
            || Instant::now() >= deadline
        {
            // No SQL was attempted; the slot still owns this original provisional
            // lease. Keep it for explicit recovery rather than recreating custody.
            return Err(E::ReserveRecovery.into());
        }
        let store = self.store.as_ref().ok_or(E::Closed)?;
        let provider = self.capacity_provider.as_ref().ok_or(E::Closed)?;
        let result = store.settle_legacy_initial_frozen(
            &admission.record,
            &admission.encoded,
            &admission.claims,
            provider,
            &mut admission.lease,
            &admission._input,
        );
        match result {
            Ok((outcome, _)) => {
                let committed = matches!(outcome, JournalInsertOutcome::Committed(_));
                admission.outcome = Some(outcome);
                if committed {
                    Ok(admission.record.operation_id)
                } else {
                    Err(E::ReserveRecovery.into())
                }
            }
            Err(error) => {
                admission.failure = Some(error);
                Err(E::ReserveRecovery.into())
            }
        }
    }

    pub(super) fn admission_observation_credit(
        &self,
        input: &InputPermit,
    ) -> Result<InputPermit, OperationJournalOwnerError> {
        use std::mem::size_of;
        let profile = self
            .final_profile_guard
            .as_ref()
            .ok_or(LegacyAdoptionCustodyError::Closed)?;
        let node = 12
            * (size_of::<(VolumeIdentity, VolumeFacts)>()
                + size_of::<(VolumeIdentity, LiveDescriptorCapacityProvenance)>())
            + 32 * size_of::<usize>();
        let mut rows = 0usize;
        let mut text = 0usize;
        let mut validation = 0usize;
        let mut observe = |reserve: &OwnedReserveAllocation| -> Option<()> {
            rows = rows.checked_add(1)?;
            validation = validation.checked_add(reserve.observation_working_bytes(profile)?)?;
            let observation = reserve.observation();
            text = text
                .checked_add(profile.profile_root_identity().len())?
                .checked_add(observation.root_identity().len())?
                .checked_add(observation.file_identity().len())?;
            Some(())
        };
        if let Some(reserve) = &self.reserve {
            observe(reserve).ok_or(LegacyAdoptionCustodyError::Changed)?;
        }
        for binding in self.source_volumes.values() {
            if let Some(reserve) = &binding.reserve {
                observe(reserve).ok_or(LegacyAdoptionCustodyError::Changed)?;
            }
        }
        if let Some(volumes) = self
            .legacy_adoption
            .as_ref()
            .and_then(|s| s.volumes.as_ref())
        {
            for entry in &volumes.entries {
                if let legacy_adoption_volumes::LegacyVolumeEntry::Owned {
                    reserve: Some(reserve),
                    ..
                } = entry
                {
                    observe(reserve).ok_or(LegacyAdoptionCustodyError::Changed)?;
                }
            }
        }
        if let Some(owner) = &self.volume_owner {
            validation = validation
                .checked_add(
                    owner
                        .revalidation_working_bytes()
                        .ok_or(LegacyAdoptionCustodyError::Changed)?,
                )
                .ok_or(LegacyAdoptionCustodyError::Changed)?;
        }
        for binding in self.source_volumes.values() {
            if let Some(owner) = &binding.owner {
                validation = validation
                    .checked_add(
                        owner
                            .revalidation_working_bytes()
                            .ok_or(LegacyAdoptionCustodyError::Changed)?,
                    )
                    .ok_or(LegacyAdoptionCustodyError::Changed)?;
            }
        }
        if let Some(volumes) = self
            .legacy_adoption
            .as_ref()
            .and_then(|s| s.volumes.as_ref())
        {
            for entry in &volumes.entries {
                if let legacy_adoption_volumes::LegacyVolumeEntry::Owned {
                    owner: Some(owner),
                    ..
                } = entry
                {
                    validation = validation
                        .checked_add(
                            owner
                                .revalidation_working_bytes()
                                .ok_or(LegacyAdoptionCustodyError::Changed)?,
                        )
                        .ok_or(LegacyAdoptionCustodyError::Changed)?;
                }
            }
        }
        for registration in self.source_registrations.values() {
            validation = validation
                .checked_add(
                    registration
                        .request
                        .registered_boundary_working_bytes(profile)
                        .ok_or(LegacyAdoptionCustodyError::Changed)?,
                )
                .ok_or(LegacyAdoptionCustodyError::Changed)?;
        }
        let bytes = rows
            .checked_mul(node + 8 * size_of::<usize>())
            .and_then(|n| text.checked_mul(2).and_then(|s| n.checked_add(s)))
            .and_then(|n| n.checked_add(validation))
            .ok_or(LegacyAdoptionCustodyError::Changed)?;
        input
            .reserve_historical_scratch(bytes)
            .map_err(OperationJournalOwnerError::from)
    }
}
