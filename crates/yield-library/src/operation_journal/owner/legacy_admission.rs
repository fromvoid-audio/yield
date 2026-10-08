//! Original worker consumer. Passive V19 bytes never reconstruct this custody.
use super::*;
use crate::operation_journal::legacy_adoption_commands::LegacyAdoptionIntentArgs;
use crate::operation_journal::{
    admission_claims::AdmissionClaim, capacity_provider::ProvisionalCapacityLease,
    store::BorrowedInitialClaimSettlementError,
};
use crate::sample_sources::db::{LegacySourceReadOwner, SourceCommandId};
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
    migration: Option<RetainedMigration>,
    bootstrap: Option<RetainedBootstrap>,
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
            migration: None,
            bootstrap: None,
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

pub(crate) struct OriginalLegacyMigrationAdmission<'a> {
    source: &'a LegacySourceReadOwner,
    profile: &'a WritableProfileGuard,
    args: &'a LegacyAdoptionIntentArgs,
    operation: OperationId,
    deadline: Instant,
    _claims: &'a AdmissionClaimInventory,
    _lease: &'a ProvisionalCapacityLease,
}
impl OriginalLegacyMigrationAdmission<'_> {
    pub(crate) fn migration_inputs(
        &self,
    ) -> (
        &WritableProfileGuard,
        &LegacyAdoptionIntentArgs,
        OperationId,
        Instant,
    ) {
        (self.profile, self.args, self.operation, self.deadline)
    }
    pub(crate) fn migrate(&self) -> crate::sample_sources::db::LegacyMigrationSettlement {
        self.source.migrate_original_admitted(self)
    }
    pub(crate) fn source_policy(&self) -> (u64, u64, u64, u64) {
        self.args.original_migration_policy()
    }
    pub(crate) fn validate_original_connection(
        &self,
        connection: &crate::sqlite_budget::OwnedConnection,
    ) -> Result<(), crate::sample_sources::db::LegacyObservationError> {
        if !self
            .source
            .matches_original_migration_connection(connection)
        {
            return Err(crate::sample_sources::db::LegacyObservationError(
                "foreign original migration connection",
            ));
        }
        self.source
            .validate_admission_checkpoint_until(self.profile, self.args, self.deadline)
    }
}
impl OwnerRuntime {
    fn original_legacy_migration_admission<'a>(
        &'a self,
        command: SourceCommandId,
        digest: [u8; 32],
        deadline: Instant,
        input: &InputPermit,
    ) -> Result<OriginalLegacyMigrationAdmission<'a>, OperationJournalOwnerError> {
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
        let admission = retained.admission.as_ref().ok_or(E::ReserveRecovery)?;
        if admission.failure.is_some()
            || !matches!(admission.outcome, Some(JournalInsertOutcome::Committed(_)))
        {
            return Err(E::ReserveRecovery.into());
        }
        let profile = self.final_profile_guard.as_ref().ok_or(E::Closed)?;
        retained
            .source
            .validate_admission_checkpoint_until(profile, &retained.args, deadline)
            .map_err(|_| E::Changed)?;
        input.fund_historical_consumer()?;
        let snapshot = self.store().rebuild_admission_snapshot_for_input(input)?;
        if !matches!(snapshot.readiness(), CapacityRebuildReadiness::Ready) {
            return Err(E::ReserveRecovery.into());
        }
        let provider = self.capacity_provider.as_ref().ok_or(E::Closed)?;
        snapshot
            .validate_provider(provider, Some(input))
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
        self.store()
            .verify_legacy_initial_frozen(&admission.record, &admission.claims, input)?;
        if Instant::now() >= deadline {
            return Err(E::Changed.into());
        }
        Ok(OriginalLegacyMigrationAdmission {
            source: &retained.source,
            profile,
            args: &retained.args,
            operation: admission.record.operation_id,
            deadline,
            _claims: &admission.claims,
            _lease: &admission.lease,
        })
    }
}

// Result allocations and all uncertainty die before the original migration input credit.
struct RetainedMigration {
    settlement: crate::sample_sources::db::LegacyMigrationSettlement,
    input: InputPermit,
}
pub(super) const MIGRATION_INPUT_BYTES: usize = 1024 * 1024;
impl OwnerRuntime {
    pub(super) fn migrate_legacy_source(
        &mut self,
        command: SourceCommandId,
        digest: [u8; 32],
        deadline: Instant,
        input: InputPermit,
    ) -> Result<OperationId, OperationJournalOwnerError> {
        use crate::sample_sources::db::LegacyMigrationSettlement as S;
        use LegacyAdoptionCustodyError as E;
        input.fund_historical_consumer()?;
        let retained = self.legacy_adoption.as_ref().ok_or(E::Closed)?;
        if retained.args.source_command_id() != command
            || retained.digest != digest
            || Instant::now() >= deadline
            || retained.input.shared.status() != OperationJournalOwnerStatus::Available
        {
            return Err(E::Changed.into());
        }
        let admission = retained.admission.as_ref().ok_or(E::ReserveRecovery)?;
        if let Some(migration) = &admission.migration {
            let S::Committed(committed) = &migration.settlement else {
                // An attempted/unknown migration is never silently replayed.
                return Err(E::ReserveRecovery.into());
            };
            self.verify_original_legacy_claims(admission, &input)?;
            let profile = self.final_profile_guard.as_ref().ok_or(E::Closed)?;
            match admission
                .bootstrap
                .as_ref()
                .map(|bootstrap| &bootstrap.settlement)
            {
                Some(crate::sample_sources::db::LegacyBootstrapSettlement::Committed(
                    bootstrap,
                )) => {
                    retained
                        .source
                        .revalidate_committed_bootstrap(
                            profile,
                            &retained.args,
                            committed,
                            bootstrap,
                            deadline,
                        )
                        .map_err(|_| E::Changed)?;
                }
                Some(crate::sample_sources::db::LegacyBootstrapSettlement::Unknown) => {
                    return Err(E::ReserveRecovery.into());
                }
                // Known bootstrap refusals/rollback do not erase the committed migration.
                _ => retained
                    .source
                    .revalidate_committed_migration(profile, &retained.args, committed, deadline)
                    .map_err(|_| E::Changed)?,
            }
            return Ok(committed.operation);
        }
        // Prove the genuine original committed admission before installing the attempted slot.
        self.original_legacy_migration_admission(command, digest, deadline, &input)?;
        self.legacy_adoption
            .as_mut()
            .ok_or(E::Closed)?
            .admission
            .as_mut()
            .ok_or(E::Closed)?
            .migration = Some(RetainedMigration {
            settlement: S::Unknown,
            input,
        });
        let settlement = {
            let input = &self
                .legacy_adoption
                .as_ref()
                .ok_or(E::Closed)?
                .admission
                .as_ref()
                .ok_or(E::Closed)?
                .migration
                .as_ref()
                .ok_or(E::Closed)?
                .input;
            match self.original_legacy_migration_admission(command, digest, deadline, input) {
                Ok(proof) => proof.migrate(),
                Err(_) => S::NotStarted,
            }
        };
        let result = match &settlement {
            S::Committed(committed) => Ok(committed.operation),
            _ => Err(E::ReserveRecovery.into()),
        };
        let migration = self
            .legacy_adoption
            .as_mut()
            .ok_or(E::Closed)?
            .admission
            .as_mut()
            .ok_or(E::Closed)?
            .migration
            .as_mut()
            .ok_or(E::Closed)?;
        if matches!(settlement, S::Committed(_)) {
            // No selected diagnostic escapes this confirmed success. Keep result backing,
            // but release its temporary reply/observer loan instead of pinning it until shutdown.
            migration.input.historical_reply = None;
        }
        migration.settlement = settlement;
        result
    }
}

// This distinct borrow is created only after committed migration readback and a fresh original
// claim/provider settlement check. Receipt bytes cannot construct it.
pub(crate) struct OriginalLegacyBootstrapAdmission<'a> {
    source: &'a LegacySourceReadOwner,
    profile: &'a WritableProfileGuard,
    args: &'a LegacyAdoptionIntentArgs,
    migration: &'a crate::sample_sources::db::VerifiedLegacyMigration,
    deadline: Instant,
    _claims: &'a AdmissionClaimInventory,
    _lease: &'a ProvisionalCapacityLease,
}
impl OriginalLegacyBootstrapAdmission<'_> {
    pub(crate) fn bootstrap_inputs(
        &self,
    ) -> (
        &WritableProfileGuard,
        &LegacyAdoptionIntentArgs,
        &crate::sample_sources::db::VerifiedLegacyMigration,
        Instant,
    ) {
        (self.profile, self.args, self.migration, self.deadline)
    }
    pub(crate) fn source_policy(&self) -> (u64, u64, u64, u64) {
        self.args.original_migration_policy()
    }
    pub(crate) fn validate_original_connection(
        &self,
        connection: &crate::sqlite_budget::OwnedConnection,
    ) -> Result<(), crate::sample_sources::db::LegacyObservationError> {
        if !self
            .source
            .matches_original_migration_connection(connection)
        {
            return Err(crate::sample_sources::db::LegacyObservationError(
                "foreign original bootstrap connection",
            ));
        }
        self.source.revalidate_committed_migration(
            self.profile,
            self.args,
            self.migration,
            self.deadline,
        )
    }
    fn bootstrap(&self) -> crate::sample_sources::db::LegacyBootstrapSettlement {
        self.source.bootstrap_original_admitted(self)
    }
}
struct RetainedBootstrap {
    settlement: crate::sample_sources::db::LegacyBootstrapSettlement,
    input: InputPermit,
}
impl OwnerRuntime {
    fn original_legacy_bootstrap_admission<'a>(
        &'a self,
        command: SourceCommandId,
        digest: [u8; 32],
        deadline: Instant,
        input: &InputPermit,
    ) -> Result<OriginalLegacyBootstrapAdmission<'a>, OperationJournalOwnerError> {
        use crate::sample_sources::db::LegacyMigrationSettlement as M;
        use LegacyAdoptionCustodyError as E;
        let retained = self.legacy_adoption.as_ref().ok_or(E::Closed)?;
        if retained.args.source_command_id() != command
            || retained.digest != digest
            || Instant::now() >= deadline
            || retained.input.shared.status() != OperationJournalOwnerStatus::Available
        {
            return Err(E::Changed.into());
        }
        let admission = retained.admission.as_ref().ok_or(E::ReserveRecovery)?;
        if admission.failure.is_some()
            || !matches!(admission.outcome, Some(JournalInsertOutcome::Committed(_)))
        {
            return Err(E::ReserveRecovery.into());
        }
        let M::Committed(migration) = &admission
            .migration
            .as_ref()
            .ok_or(E::ReserveRecovery)?
            .settlement
        else {
            return Err(E::ReserveRecovery.into());
        };
        self.verify_original_legacy_claims(admission, input)?;
        let proof = OriginalLegacyBootstrapAdmission {
            source: &retained.source,
            profile: self.final_profile_guard.as_ref().ok_or(E::Closed)?,
            args: &retained.args,
            migration,
            deadline,
            _claims: &admission.claims,
            _lease: &admission.lease,
        };
        // The actual Source validates the exact retained connection again when entering scope.
        retained
            .source
            .revalidate_committed_migration(proof.profile, proof.args, migration, deadline)
            .map_err(|_| E::Changed)?;
        Ok(proof)
    }

    fn verify_original_legacy_claims(
        &self,
        admission: &Retained,
        input: &InputPermit,
    ) -> Result<(), OperationJournalOwnerError> {
        use LegacyAdoptionCustodyError as E;
        input.fund_historical_consumer()?;
        self.store()
            .verify_legacy_initial_frozen(&admission.record, &admission.claims, input)?;
        let snapshot = self.store().rebuild_admission_snapshot_for_input(input)?;
        let provider = self.capacity_provider.as_ref().ok_or(E::Closed)?;
        snapshot
            .validate_provider(provider, Some(input))
            .map_err(|_| E::ReserveRecovery)?;
        provider
            .validate_recovery_state()
            .map_err(|_| E::ReserveRecovery)?;
        if !matches!(snapshot.readiness(), CapacityRebuildReadiness::Ready)
            || admission.claims.claims().iter().any(|claim| {
                !snapshot.claims().iter().any(|row| {
                    row.claim_id() == claim.claim_id()
                        && row.operation_id() == admission.record.operation_id
                        && row.volume() == claim.volume()
                        && row.allocation_class() == claim.allocation_class()
                        && row.allocated_bytes() == claim.allocated_bytes()
                        && row.release_id().is_none()
                        && !row.audit_pending()
                })
            })
        {
            return Err(E::ReserveRecovery.into());
        }
        Ok(())
    }

    pub(super) fn bootstrap_legacy_source(
        &mut self,
        command: SourceCommandId,
        digest: [u8; 32],
        deadline: Instant,
        input: InputPermit,
    ) -> Result<OperationId, OperationJournalOwnerError> {
        use crate::sample_sources::db::{
            LegacyBootstrapSettlement as B, LegacyMigrationSettlement as M,
        };
        use LegacyAdoptionCustodyError as E;
        let retained = self.legacy_adoption.as_ref().ok_or(E::Closed)?;
        if retained.args.source_command_id() != command
            || retained.digest != digest
            || Instant::now() >= deadline
            || retained.input.shared.status() != OperationJournalOwnerStatus::Available
        {
            return Err(E::Changed.into());
        }
        let admission = retained.admission.as_ref().ok_or(E::ReserveRecovery)?;
        if let Some(bootstrap) = &admission.bootstrap {
            let B::Committed(bootstrap) = &bootstrap.settlement else {
                return Err(E::ReserveRecovery.into());
            };
            let M::Committed(migration) = &admission
                .migration
                .as_ref()
                .ok_or(E::ReserveRecovery)?
                .settlement
            else {
                return Err(E::ReserveRecovery.into());
            };
            self.verify_original_legacy_claims(admission, &input)?;
            retained
                .source
                .revalidate_committed_bootstrap(
                    self.final_profile_guard.as_ref().ok_or(E::Closed)?,
                    &retained.args,
                    migration,
                    bootstrap,
                    deadline,
                )
                .map_err(|_| E::Changed)?;
            return Ok(bootstrap.operation);
        }
        self.original_legacy_bootstrap_admission(command, digest, deadline, &input)?;
        self.legacy_adoption
            .as_mut()
            .ok_or(E::Closed)?
            .admission
            .as_mut()
            .ok_or(E::Closed)?
            .bootstrap = Some(RetainedBootstrap {
            settlement: B::Unknown,
            input,
        });
        let settlement = {
            let input = &self
                .legacy_adoption
                .as_ref()
                .ok_or(E::Closed)?
                .admission
                .as_ref()
                .ok_or(E::Closed)?
                .bootstrap
                .as_ref()
                .ok_or(E::Closed)?
                .input;
            match self.original_legacy_bootstrap_admission(command, digest, deadline, input) {
                Ok(proof) => proof.bootstrap(),
                Err(_) => B::NotStarted,
            }
        };
        let result = match &settlement {
            B::Committed(bootstrap) => Ok(bootstrap.operation),
            _ => Err(E::ReserveRecovery.into()),
        };
        let bootstrap = self
            .legacy_adoption
            .as_mut()
            .ok_or(E::Closed)?
            .admission
            .as_mut()
            .ok_or(E::Closed)?
            .bootstrap
            .as_mut()
            .ok_or(E::Closed)?;
        if matches!(settlement, B::Committed(_)) {
            bootstrap.input.historical_reply = None;
        }
        bootstrap.settlement = settlement;
        result
    }
}
