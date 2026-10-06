//! Historical exact original retirement observations, never qualified physical authority.
use super::*;
use crate::app_dirs::WritableProfileGuard;
use crate::operation_journal::{
    AdmissionClaimInventory, FullCapacityPlan, ParticipantSet,
    database_claims::{
        ClaimDirection, DatabaseBinding, DatabaseClaimInventory, DatabaseClaimSlice,
        DatabaseParticipant, DatabaseRole, InventoryHeader, RevisionDomain, SourceBinding,
        retirement_forward_policy::registered_slice_identity,
    },
    full_capacity::FullCapacityAllocationClass as Class,
    source_registry_retirement::SourceRegistryRetirementIntentArgs,
};
use uuid::Uuid;
const FIXED: usize = 8 * 1024;
struct Backing {
    inventory: DatabaseClaimInventory,
    encoded: Vec<u8>,
    _journal_input: JournalInput,
    _credit: HandshakeCredit,
}
pub(in crate::operation_journal) struct Observation(Option<Arc<Backing>>);
impl Clone for Observation {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl Drop for Observation {
    fn drop(&mut self) {
        if let Some(backing) = self.0.take().and_then(Arc::into_inner) {
            drop(backing);
        }
    }
}
impl Observation {
    pub(in crate::operation_journal) fn inventory(&self) -> &DatabaseClaimInventory {
        &self
            .0
            .as_deref()
            .expect("live original inventory")
            .inventory
    }
    pub(in crate::operation_journal) fn encoded(&self) -> &[u8] {
        &self.0.as_deref().expect("live original inventory").encoded
    }
}
fn invalid() -> WalPreparationError {
    WalPreparationError::InvalidInput
}
fn full() -> WalPreparationError {
    WalPreparationError::Full
}
fn sum(values: &[usize]) -> Result<usize, WalPreparationError> {
    values
        .iter()
        .try_fold(0usize, |n, v| n.checked_add(*v))
        .ok_or_else(full)
}
impl Collector {
    fn inventory_context(
        &self,
        nonce: Uuid,
    ) -> Result<
        (
            &GlobalPreparationObservation,
            &SourceLeaseWalTransportObservation,
        ),
        WalPreparationError,
    > {
        if self.decision_nonce() != Some(nonce)
            || !self.checked_decision_acknowledged()?
            || self.reason.is_some()
            || self.done
            || self.shared.canceled.load(Ordering::Acquire)
            || Instant::now() >= self.input.deadline
            || self.held.len() != 2
        {
            return Err(invalid());
        }
        let snapshot = self.snapshot.as_ref().ok_or_else(invalid)?;
        let [
            WalPreparedDatabase::Global(global),
            WalPreparedDatabase::Lease(source),
        ] = snapshot.databases()
        else {
            return Err(invalid());
        };
        if !source.registered_custody_was_retained() {
            return Err(invalid());
        }
        Ok((global, source))
    }
    pub(in crate::operation_journal) fn original_retirement_inventory(
        &self,
        profile: &WritableProfileGuard,
        nonce: Uuid,
        args: &SourceRegistryRetirementIntentArgs,
        participants: &ParticipantSet,
        plan: &FullCapacityPlan,
        claims: &AdmissionClaimInventory,
    ) -> Result<Observation, WalPreparationError> {
        #[cfg(all(test, target_os = "linux"))]
        if let Some(hook) = self.shared.retirement_hook_for_tests.lock().unwrap().take() {
            hook(nonce);
        }
        let (global, source) = self.inventory_context(nonce)?;
        let fence = source.original_binding().stored_fence();
        let encoding = fence.existing_encoding_bound().ok_or_else(full)?;
        let roots = profile
            .profile_root()
            .as_os_str()
            .as_encoded_bytes()
            .len()
            .checked_mul(2)
            .ok_or_else(full)?;
        let retained = sum(&[
            std::mem::size_of::<Backing>(),
            64,
            2 * std::mem::size_of::<DatabaseBinding>(),
            2 * std::mem::size_of::<DatabaseParticipant>(),
            6 * std::mem::size_of::<DatabaseClaimSlice>(),
            fence
                .retained_heap_bytes()
                .ok_or_else(full)?
                .checked_mul(3)
                .ok_or_else(full)?,
            encoding,
            profile.profile_root_identity().len(),
            profile.owner_identity().len(),
            global.database_identity().len(),
            global
                .wal_scope()
                .ok_or_else(invalid)?
                .root_identity()
                .len()
                .checked_mul(2)
                .ok_or_else(full)?,
        ])?;
        let validation = args.validation_scratch_bound().ok_or_else(full)?;
        let projection_peak = sum(&[
            FIXED,
            roots,
            retained,
            validation.max(encoding.checked_mul(2).ok_or_else(full)?),
        ])?;
        let mut credit = self.input.budget.credit();
        credit.resize(sum(&[FIXED, roots])?).map_err(|_| full())?;
        credit.resize(projection_peak).map_err(|_| full())?;
        profile.validate_current().map_err(|_| invalid())?;
        self.compare_retirement_scope(args, participants, plan, claims, self.input.digest, None)?;
        let command_digest = args.canonical_command_digest().map_err(|_| invalid())?;
        let (global, source) = self.inventory_context(nonce)?;
        let fence = source.original_binding().stored_fence();
        let subject = fence.subject();
        let scope = global.wal_scope().ok_or_else(invalid)?;
        let generation = source.original_binding().registration_generation();
        let registration = subject
            .registration()
            .ok_or_else(invalid)?
            .registration_id();
        let header = InventoryHeader {
            profile: profile.profile_root_identity().into(),
            acquisition: profile.owner_identity().into(),
            ownership_generation: profile.ownership_generation(),
            decision_nonce: nonce,
        };
        let source_binding = DatabaseBinding {
            id: registration,
            role: DatabaseRole::Source,
            volume: fence.device(),
            allocation_unit: fence.allocation_unit(),
            main_file: subject.database_identity().into(),
            registration,
            owner_generation: subject.lease_epoch(),
            lifecycle: subject.lifecycle_generation(),
            database_root: subject.database_root_identity().into(),
            root_binding: subject.root_binding_identity().into(),
            source: Some(SourceBinding::from_lease_fence(fence).map_err(|_| invalid())?),
            current_revision: subject.scan_revision(),
            participants: vec![DatabaseParticipant {
                id: args.source_participant().as_uuid(),
                requirement: 1,
                revision_domain: RevisionDomain::SourceLeaseLifecycle,
                command_digest,
                before: subject.lease_epoch(),
                after: subject.lease_epoch(),
            }],
        };
        let global_binding = DatabaseBinding {
            id: global.owner().registration(),
            role: DatabaseRole::Global,
            volume: scope.main().device(),
            allocation_unit: scope.main().allocation_unit(),
            main_file: global.database_identity().into(),
            registration: global.owner().registration(),
            owner_generation: global.owner().generation(),
            lifecycle: global.owner().generation(),
            database_root: scope.root_identity().into(),
            root_binding: scope.root_identity().into(),
            source: None,
            current_revision: global.mutation_revision(),
            participants: vec![DatabaseParticipant {
                id: args.draining_work().participant_id().as_uuid(),
                requirement: 1,
                revision_domain: RevisionDomain::GlobalMutationGeneration,
                command_digest,
                before: global.mutation_revision(),
                after: global
                    .mutation_revision()
                    .checked_add(2)
                    .filter(|n| *n <= i64::MAX as u64)
                    .ok_or_else(invalid)?,
            }],
        };
        let source_per = source
            .original_binding()
            .retirement_forward_policy()
            .ok_or_else(invalid)?
            .component_ceilings();
        let global_per = scope
            .retirement_forward_policy()
            .ok_or_else(invalid)?
            .per_transition_ceilings();
        let family = args
            .initial_forward_policy_digest()
            .map_err(|_| invalid())?;
        let mut slices = Vec::with_capacity(6);
        for (work, binding, direction, classes, per) in [
            (
                args.source_work(),
                &source_binding,
                ClaimDirection::LeaseMaintenance,
                [Class::SourceDatabaseCommit, Class::SourceWalShm],
                source_per,
            ),
            (
                args.draining_work(),
                &global_binding,
                ClaimDirection::RequiredForward,
                [Class::GlobalDatabaseCommit, Class::GlobalWalShm],
                global_per,
            ),
            (
                args.retired_work(),
                &global_binding,
                ClaimDirection::RequiredForward,
                [Class::GlobalDatabaseCommit, Class::GlobalWalShm],
                global_per,
            ),
        ] {
            for (component, class) in classes.into_iter().enumerate() {
                let claim = claims
                    .claims()
                    .iter()
                    .find(|c| c.allocation_class() == class)
                    .ok_or_else(invalid)?
                    .claim_id()
                    .as_uuid();
                let id = registered_slice_identity(
                    &header, binding, generation, claim, &family, work, direction, class,
                )
                .map_err(|_| invalid())?;
                slices.push(DatabaseClaimSlice {
                    id,
                    claim,
                    binding: binding.id,
                    participant: work.participant_id().as_uuid(),
                    direction,
                    class,
                    ceiling: if component == 0 {
                        per[0]
                    } else {
                        per[1].checked_add(per[2]).ok_or_else(invalid)?
                    },
                    wal_ceiling: if component == 0 { 0 } else { per[1] },
                    shm_ceiling: if component == 0 { 0 } else { per[2] },
                });
            }
        }
        let inventory = DatabaseClaimInventory::new_registered_retirement_forward(
            header,
            vec![source_binding, global_binding],
            slices,
            generation,
        )
        .map_err(|_| invalid())?;
        inventory
            .validate_against(plan, claims)
            .map_err(|_| invalid())?;
        let encoded = inventory.encode().map_err(|_| invalid())?;
        profile.validate_current().map_err(|_| invalid())?;
        self.compare_retirement_scope(args, participants, plan, claims, self.input.digest, None)?;
        let (global, source) = self.inventory_context(nonce)?;
        let source_binding = inventory
            .bindings()
            .iter()
            .find(|b| b.role == DatabaseRole::Source)
            .ok_or_else(invalid)?;
        let global_binding = inventory
            .bindings()
            .iter()
            .find(|b| b.role == DatabaseRole::Global)
            .ok_or_else(invalid)?;
        if inventory.header().profile != profile.profile_root_identity()
            || inventory.header().acquisition != profile.owner_identity()
            || inventory.header().ownership_generation != profile.ownership_generation()
            || inventory.header().decision_nonce != nonce
            || inventory.source_registration_generation()
                != Some(source.original_binding().registration_generation())
            || source_binding
                .source
                .as_ref()
                .and_then(|s| s.lease_fence.as_ref())
                != Some(source.original_binding().stored_fence())
            || global_binding.registration != global.owner().registration()
            || global_binding.owner_generation != global.owner().generation()
            || global_binding.lifecycle != global.owner().generation()
            || global_binding.current_revision != global.mutation_revision()
            || global_binding.database_root
                != global.wal_scope().ok_or_else(invalid)?.root_identity()
            || global_binding.root_binding != global_binding.database_root
        {
            return Err(invalid());
        }
        let observation = Observation(Some(Arc::new(Backing {
            inventory,
            encoded,
            _journal_input: self._journal_input.clone(),
            _credit: credit,
        })));
        #[cfg(test)]
        {
            *self.shared.retirement_for_tests.lock().unwrap() = Some(observation.clone());
        }
        Ok(observation)
    }
}
