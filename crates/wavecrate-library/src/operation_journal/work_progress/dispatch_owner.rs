//! Exclusive in-memory ownership for one frozen work-progress dispatch lifecycle.
//!
//! `LiveDispatchOwner` is the narrow owner boundary between a future admission/provider
//! integration and [`super::WorkProgress`].  It has no production constructor in this module:
//! the test-only constructor exists to exercise the protocol while real profile ownership,
//! admission, and provider capabilities remain separate contracts.  An admission-claim
//! observation supplied to progress is never treated as a capacity reservation here.
//!
//! The owner tracks every issued key, including source reads that are intentionally absent from
//! the cancellation dispatch manifest.  Dropping a permit leaves its ledger entry outstanding;
//! only an explicit owner-mediated retirement closes that entry.  The private sealed barrier is
//! an in-memory observation binding and is not wired to settlement, terminal progress, or
//! filesystem execution.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;

use crate::operation_journal::CancellationDispatchOutcomeRow;

use super::{
    CancellationCoordinatorClosureObservationRecord, CancellationDispatchOutcome,
    CancellationDispatchOutcomeRecord, CancellationDispatchOutcomeRecordError,
    CancellationNamespaceCleanupObservationRecord, CancellationRequestObservation,
    CancellationResourceCleanupObservationRecord, CancellationSourceClosureObservationRecord,
    CompletionContext, FullProgressDisposition, OperationId, OperationPhase, ParticipantScope,
    ProgressApplyOutcome, ProgressStepStatus, StoredStepEvidence, WorkPlan, WorkProgress,
    WorkProgressError, WorkUnitApplicability, WorkUnitKey,
};

static NEXT_OWNER_ID: AtomicU64 = AtomicU64::new(1);

/// Lifecycle of one exclusive in-memory dispatch owner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DispatchOwnerLifecycle {
    /// New applicable work may be reserved and entered.
    Open,
    /// Cancellation is frozen; only outstanding permits may be retired.
    CancelDraining,
    /// Every issued permit returned and the private cancellation barrier sealed.
    Sealed,
}

/// Failure from the bounded in-memory dispatch-owner protocol.
#[derive(Debug, Error, PartialEq)]
pub(super) enum DispatchOwnerError {
    /// The owner identity counter could not advance without wrapping.
    #[error("dispatch owner identity exhausted")]
    OwnerIdentityExhausted,
    /// The test-only owner fixture was not derived from initial, untouched progress.
    #[error("dispatch owner test seed must be fresh initial work progress")]
    NonFreshProgressSeed,
    /// The requested lifecycle operation is closed after cancellation or sealing.
    #[error("dispatch owner is no longer open for issuance or entry")]
    IssuanceClosed,
    /// Cancellation was required before sealing a barrier.
    #[error("dispatch owner cancellation has not been requested")]
    CancellationNotRequested,
    /// An audit overlay prevents a private barrier from being sealed.
    #[error("dispatch owner cannot seal while an audit overlay is active")]
    AuditActive,
    /// A key was absent from the frozen plan.
    #[error("dispatch owner key is unknown: {key:?}")]
    UnknownWorkUnit { key: WorkUnitKey },
    /// A key was fixed not-applicable by the frozen plan.
    #[error("dispatch owner key is not applicable: {key:?}")]
    NotApplicable { key: WorkUnitKey },
    /// A key was no longer pending in progress.
    #[error("dispatch owner key is not pending: {key:?}")]
    NotPending { key: WorkUnitKey },
    /// A key has already had its one bounded attempt issued.
    #[error("dispatch owner key already has an issued attempt: {key:?}")]
    AlreadyIssued { key: WorkUnitKey },
    /// The current participant scope was absent for a planned key.
    #[error("dispatch owner participant scope is missing for {key:?}")]
    ScopeMissing { key: WorkUnitKey },
    /// A prerequisite was not completed or declared not applicable.
    #[error("dispatch owner prerequisite {prerequisite:?} is incomplete for {dependent:?}")]
    PrerequisiteIncomplete {
        /// Incomplete prerequisite key.
        prerequisite: WorkUnitKey,
        /// Dependent key that cannot yet be issued.
        dependent: WorkUnitKey,
    },
    /// The owner attempt counter could not advance without wrapping.
    #[error("dispatch owner attempt id exhausted")]
    AttemptIdExhausted,
    /// A permit came from another live owner.
    #[error("dispatch permit owner identity does not match")]
    PermitOwnerMismatch,
    /// A permit carried another operation identity.
    #[error("dispatch permit operation identity does not match")]
    PermitOperationMismatch,
    /// A permit carried a stale execution epoch.
    #[error("dispatch permit epoch {actual} does not match current epoch {expected}")]
    PermitEpochMismatch {
        /// Current owner epoch.
        expected: u64,
        /// Epoch retained by the permit.
        actual: u64,
    },
    /// A permit key is not the exact key tracked by its owner ledger.
    #[error("dispatch permit key is not tracked: {key:?}")]
    PermitNotTracked { key: WorkUnitKey },
    /// A permit was used after its ledger state changed.
    #[error("dispatch permit state is no longer valid for {key:?}")]
    PermitStateMismatch { key: WorkUnitKey },
    /// A sealed barrier no longer matches the owner's retained sealed outcome.
    #[error("dispatch barrier state does not match the owner's sealed outcome")]
    BarrierStateMismatch,
    /// A sealed barrier no longer matches the owner's frozen cancellation inventory.
    #[error("dispatch barrier cancellation inventory does not match the owner's current inventory")]
    BarrierInventoryMismatch,
    /// The owner's accepted after-effect state does not retain the barrier's sealed dispatch.
    #[error("dispatch barrier does not match the owner's accepted after-effect dispatch")]
    AfterEffectDispatchMismatch,
    /// An entered or reserved permit remains outstanding at the cancellation barrier.
    #[error("dispatch owner has {count} outstanding permit(s)")]
    OutstandingPermits { count: usize },
    /// A normal completion was attempted after cancellation closed normal dispatch.
    #[error("dispatch owner normal completion is closed after cancellation")]
    NormalCompletionClosed,
    /// A late retirement was attempted before cancellation.
    #[error("dispatch owner late retirement requires cancellation")]
    LateRetirementBeforeCancellation,
    /// A normal completion unexpectedly remained in the frozen cancellation manifest.
    #[error("normal completion remained in cancellation dispatch manifest for {key:?}")]
    NormalCompletionInManifest { key: WorkUnitKey },
    /// The cancellation barrier generation could not advance without wrapping.
    #[error("dispatch owner cancellation generation exhausted")]
    RecoveryGenerationExhausted,
    /// The underlying progress guard rejected an owner operation.
    #[error(transparent)]
    Progress(#[from] WorkProgressError),
    /// The frozen dispatch manifest could not be derived.
    #[error(transparent)]
    DispatchManifest(#[from] super::CancellationDispatchManifestError),
    /// The sealed outcome record failed bounded validation.
    #[error(transparent)]
    DispatchOutcome(#[from] CancellationDispatchOutcomeRecordError),
}

#[derive(Clone, Debug)]
struct PermitBinding {
    owner_id: u64,
    operation_id: OperationId,
    key: WorkUnitKey,
    scope: ParticipantScope,
    execution_epoch: u64,
    attempt_id: u64,
}

/// Opaque reservation permit for one exact pending work key.
///
/// This type is intentionally non-`Clone` and non-serialized.  Dropping it does not alter the
/// owner's ledger or prove that the work was never entered.  The owner identity and owner-owned
/// immutable progress bind its plan transitively, so each permit does not clone the full plan.
#[derive(Debug)]
pub(super) struct DispatchPermit {
    binding: PermitBinding,
}

impl DispatchPermit {
    /// Return the exact key bound to this reservation.
    pub(super) const fn key(&self) -> WorkUnitKey {
        self.binding.key
    }

    /// Return the monotonic attempt identifier bound to this reservation.
    pub(super) const fn attempt_id(&self) -> u64 {
        self.binding.attempt_id
    }
}

/// Opaque permit for one entered work attempt.
///
/// This type is intentionally non-`Clone` and non-serialized.  Dropping it leaves the owner
/// ledger in an entered/uncertain state and therefore cannot establish `NeverEntered`.  Its
/// owner identity binds it to the owner's immutable plan transitively.
#[derive(Debug)]
pub(super) struct EnteredDispatchPermit {
    binding: PermitBinding,
}

impl EnteredDispatchPermit {
    /// Return the exact key bound to this entered attempt.
    pub(super) const fn key(&self) -> WorkUnitKey {
        self.binding.key
    }

    /// Return the monotonic attempt identifier bound to this entered attempt.
    pub(super) const fn attempt_id(&self) -> u64 {
        self.binding.attempt_id
    }
}

/// Validation failure that returns the original opaque permit for explicit retry or retirement.
///
/// Successful transitions consume their input token.  A failed validation leaves the owner's
/// ledger untouched and returns the same token so dropping a failed operation cannot silently
/// establish non-entry or quiescence.
#[derive(Debug)]
pub(super) struct PermitFailure<P> {
    error: DispatchOwnerError,
    permit: P,
}

impl<P> PermitFailure<P> {
    fn new(error: DispatchOwnerError, permit: P) -> Self {
        Self { error, permit }
    }

    /// Borrow the validation error without consuming the recoverable permit.
    pub(super) fn error(&self) -> &DispatchOwnerError {
        &self.error
    }

    /// Recover both the validation error and original permit.
    pub(super) fn into_parts(self) -> (DispatchOwnerError, P) {
        (self.error, self.permit)
    }
}

/// Private current-epoch cancellation barrier produced after every issued permit is retired.
///
/// This value is an in-memory owner binding only.  It does not settle claims, transition
/// terminal progress, authorize cleanup, or grant filesystem capability.
pub(super) struct VerifiedCancellationBarrier {
    owner_id: u64,
    operation_id: OperationId,
    plan: WorkPlan,
    execution_epoch: u64,
    cancellation_inventory: super::CancellationInventory,
    dispatch_outcome: CancellationDispatchOutcomeRecord,
}

impl VerifiedCancellationBarrier {
    /// Borrow the exact validated dispatch outcome retained by this barrier.
    pub(super) fn dispatch_outcome(&self) -> &CancellationDispatchOutcomeRecord {
        &self.dispatch_outcome
    }

    /// Return the current epoch bound to this barrier.
    pub(super) const fn execution_epoch(&self) -> u64 {
        self.execution_epoch
    }

    /// Return the operation identity bound to this barrier.
    pub(super) const fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    /// Validate that this private barrier still belongs to the exact owner state.
    pub(super) fn validate_for_owner(
        &self,
        owner: &LiveDispatchOwner,
    ) -> Result<(), DispatchOwnerError> {
        if self.owner_id != owner.owner_id {
            return Err(DispatchOwnerError::PermitOwnerMismatch);
        }
        if self.operation_id != owner.progress.operation_id
            || self.plan != owner.progress.plan
            || self.execution_epoch != owner.progress.execution_epoch
        {
            return Err(DispatchOwnerError::PermitOperationMismatch);
        }
        if owner.lifecycle != DispatchOwnerLifecycle::Sealed {
            return Err(DispatchOwnerError::IssuanceClosed);
        }
        if owner
            .sealed_dispatch_outcome
            .as_ref()
            .is_none_or(|record| record != &self.dispatch_outcome)
        {
            return Err(DispatchOwnerError::BarrierStateMismatch);
        }
        if owner
            .progress
            .cancellation_inventory
            .as_ref()
            .is_none_or(|inventory| inventory != &self.cancellation_inventory)
        {
            return Err(DispatchOwnerError::BarrierInventoryMismatch);
        }
        Ok(())
    }

    /// Borrow the frozen cancellation inventory retained by this private binding.
    pub(super) fn cancellation_inventory(&self) -> &super::CancellationInventory {
        &self.cancellation_inventory
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DispatchLedgerState {
    Reserved,
    Entered,
    ReturnedNeverEntered,
    ReturnedUncertain,
    CompletedNormally,
    CompletedAfterCancellation,
}

enum DispatchLedgerEntry {
    Reserved {
        attempt_id: u64,
    },
    Entered {
        attempt_id: u64,
    },
    ReturnedNeverEntered {
        attempt_id: u64,
    },
    ReturnedUncertain {
        attempt_id: u64,
    },
    CompletedNormally {
        attempt_id: u64,
    },
    CompletedAfterCancellation {
        attempt_id: u64,
        evidence: StoredStepEvidence,
        context: CompletionContext,
    },
}

impl DispatchLedgerEntry {
    fn state(&self) -> DispatchLedgerState {
        match self {
            Self::Reserved { .. } => DispatchLedgerState::Reserved,
            Self::Entered { .. } => DispatchLedgerState::Entered,
            Self::ReturnedNeverEntered { .. } => DispatchLedgerState::ReturnedNeverEntered,
            Self::ReturnedUncertain { .. } => DispatchLedgerState::ReturnedUncertain,
            Self::CompletedNormally { .. } => DispatchLedgerState::CompletedNormally,
            Self::CompletedAfterCancellation { .. } => {
                DispatchLedgerState::CompletedAfterCancellation
            }
        }
    }

    fn attempt_id(&self) -> u64 {
        match self {
            Self::Reserved { attempt_id }
            | Self::Entered { attempt_id }
            | Self::ReturnedNeverEntered { attempt_id }
            | Self::ReturnedUncertain { attempt_id }
            | Self::CompletedNormally { attempt_id }
            | Self::CompletedAfterCancellation { attempt_id, .. } => *attempt_id,
        }
    }
}

/// Exclusive live owner for one in-memory `WorkProgress` instance.
///
/// There is intentionally no production constructor.  The only constructor is compiled for
/// unit tests until a real profile/provider admission boundary can issue this owner.
#[allow(dead_code)]
pub(super) struct LiveDispatchOwner {
    owner_id: u64,
    progress: WorkProgress,
    ledger: BTreeMap<WorkUnitKey, DispatchLedgerEntry>,
    lifecycle: DispatchOwnerLifecycle,
    next_attempt_id: u64,
    sealed_dispatch_outcome: Option<CancellationDispatchOutcomeRecord>,
}

impl LiveDispatchOwner {
    fn allocate_owner_id() -> Result<u64, DispatchOwnerError> {
        NEXT_OWNER_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .map_err(|_| DispatchOwnerError::OwnerIdentityExhausted)
    }

    #[cfg(test)]
    pub(super) fn new_for_test(progress: WorkProgress) -> Result<Self, DispatchOwnerError> {
        if progress.execution_epoch != 0
            || progress.phase != OperationPhase::IntentDurable
            || progress.disposition != FullProgressDisposition::None
            || progress.retry_observation.is_some()
            || progress.last_retry_observation.is_some()
            || progress.active_audit.is_some()
            || progress.audit_generation != 0
            || progress.cancellation_request.is_some()
            || progress.cancellation_inventory.is_some()
            || progress.claim_settlement.is_some()
            || progress.recovery_generation != 0
            || progress.pre_publish_cancellation.is_some()
            || progress.after_effect_reconciliation.is_some()
            || progress.cancellation_cleanup_observations.is_some()
            || progress.steps.iter().any(|step| {
                !matches!(
                    step.status,
                    ProgressStepStatus::Pending | ProgressStepStatus::NotApplicable
                ) || step.evidence.is_some()
                    || step.completion_context.is_some()
            })
        {
            return Err(DispatchOwnerError::NonFreshProgressSeed);
        }
        Ok(Self {
            owner_id: Self::allocate_owner_id()?,
            progress,
            ledger: BTreeMap::new(),
            lifecycle: DispatchOwnerLifecycle::Open,
            next_attempt_id: 0,
            sealed_dispatch_outcome: None,
        })
    }

    /// Borrow progress read-only; no mutable progress or constructor is exposed.
    pub(super) fn progress(&self) -> &WorkProgress {
        &self.progress
    }

    /// Return the owner lifecycle.
    pub(super) const fn lifecycle(&self) -> DispatchOwnerLifecycle {
        self.lifecycle
    }

    /// Reserve one exact applicable pending key whose prerequisites are closed.
    pub(super) fn issue(&mut self, key: WorkUnitKey) -> Result<DispatchPermit, DispatchOwnerError> {
        if self.lifecycle != DispatchOwnerLifecycle::Open {
            return Err(DispatchOwnerError::IssuanceClosed);
        }
        if self.progress.cancellation_request().is_some()
            || self.progress.disposition().is_terminal()
            || self.progress.phase() == OperationPhase::Terminal
        {
            return Err(DispatchOwnerError::IssuanceClosed);
        }
        if self.progress.audit_observation().is_some()
            || self.progress.disposition() == FullProgressDisposition::AuditRequired
        {
            return Err(DispatchOwnerError::AuditActive);
        }
        if self.ledger.contains_key(&key) {
            return Err(DispatchOwnerError::AlreadyIssued { key });
        }
        let unit = self
            .progress
            .plan
            .units()
            .iter()
            .find(|unit| unit.key() == key)
            .ok_or(DispatchOwnerError::UnknownWorkUnit { key })?;
        if unit.applicability() != WorkUnitApplicability::Required {
            return Err(DispatchOwnerError::NotApplicable { key });
        }
        let step = self
            .progress
            .step_for(key)
            .ok_or(DispatchOwnerError::UnknownWorkUnit { key })?;
        if step.status() != ProgressStepStatus::Pending {
            return Err(DispatchOwnerError::NotPending { key });
        }
        let scope = self
            .progress
            .scope_for(key)
            .ok_or(DispatchOwnerError::ScopeMissing { key })?
            .clone();
        for edge in self
            .progress
            .plan
            .prerequisites()
            .iter()
            .filter(|edge| edge.dependent() == key)
        {
            let prerequisite = self.progress.step_for(edge.prerequisite()).ok_or(
                DispatchOwnerError::PrerequisiteIncomplete {
                    prerequisite: edge.prerequisite(),
                    dependent: key,
                },
            )?;
            if prerequisite.status() != ProgressStepStatus::Completed
                && prerequisite.status() != ProgressStepStatus::NotApplicable
            {
                return Err(DispatchOwnerError::PrerequisiteIncomplete {
                    prerequisite: prerequisite.key(),
                    dependent: key,
                });
            }
        }
        let attempt_id = self
            .next_attempt_id
            .checked_add(1)
            .ok_or(DispatchOwnerError::AttemptIdExhausted)?;
        self.next_attempt_id = attempt_id;
        let binding = PermitBinding {
            owner_id: self.owner_id,
            operation_id: self.progress.operation_id,
            key,
            scope,
            execution_epoch: self.progress.execution_epoch,
            attempt_id,
        };
        self.ledger
            .insert(key, DispatchLedgerEntry::Reserved { attempt_id });
        Ok(DispatchPermit { binding })
    }

    /// Enter one reserved permit while issuance remains open.
    pub(super) fn enter(
        &mut self,
        permit: DispatchPermit,
    ) -> Result<EnteredDispatchPermit, PermitFailure<DispatchPermit>> {
        if self.lifecycle != DispatchOwnerLifecycle::Open {
            return Err(PermitFailure::new(
                DispatchOwnerError::IssuanceClosed,
                permit,
            ));
        }
        if let Err(error) = self.validate_binding(&permit.binding) {
            return Err(PermitFailure::new(error, permit));
        }
        if let Err(error) = self.require_ledger_state(
            permit.key(),
            permit.attempt_id(),
            DispatchLedgerState::Reserved,
        ) {
            return Err(PermitFailure::new(error, permit));
        }
        let binding = permit.binding.clone();
        self.ledger.insert(
            permit.key(),
            DispatchLedgerEntry::Entered {
                attempt_id: permit.attempt_id(),
            },
        );
        Ok(EnteredDispatchPermit { binding })
    }

    /// Complete one entered permit through the existing guarded normal-completion path.
    ///
    /// Evidence is validated before the ledger state changes.  An error leaves the entered
    /// permit available for explicit retirement or a corrected completion.
    pub(super) fn complete(
        &mut self,
        permit: EnteredDispatchPermit,
        evidence: StoredStepEvidence,
    ) -> Result<ProgressApplyOutcome, PermitFailure<EnteredDispatchPermit>> {
        if self.lifecycle != DispatchOwnerLifecycle::Open {
            return Err(PermitFailure::new(
                DispatchOwnerError::NormalCompletionClosed,
                permit,
            ));
        }
        if let Err(error) = self.validate_binding(&permit.binding) {
            return Err(PermitFailure::new(error, permit));
        }
        if let Err(error) = self.require_ledger_state(
            permit.key(),
            permit.attempt_id(),
            DispatchLedgerState::Entered,
        ) {
            return Err(PermitFailure::new(error, permit));
        }
        let outcome = match self
            .progress
            .apply_verified_completion(super::VerifiedStepCompletion {
                operation_id: permit.binding.operation_id,
                plan: self.progress.plan.clone(),
                key: permit.binding.key,
                scope: permit.binding.scope.clone(),
                execution_epoch: permit.binding.execution_epoch,
                evidence,
            }) {
            Ok(outcome) => outcome,
            Err(error) => {
                return Err(PermitFailure::new(
                    DispatchOwnerError::Progress(error),
                    permit,
                ));
            }
        };
        self.ledger.insert(
            permit.key(),
            DispatchLedgerEntry::CompletedNormally {
                attempt_id: permit.attempt_id(),
            },
        );
        Ok(outcome)
    }

    /// Explicitly return a reserved permit that never entered work.
    pub(super) fn return_reserved(
        &mut self,
        permit: DispatchPermit,
    ) -> Result<(), PermitFailure<DispatchPermit>> {
        if let Err(error) = self.validate_binding(&permit.binding) {
            return Err(PermitFailure::new(error, permit));
        }
        if let Err(error) = self.require_ledger_state(
            permit.key(),
            permit.attempt_id(),
            DispatchLedgerState::Reserved,
        ) {
            return Err(PermitFailure::new(error, permit));
        }
        self.ledger.insert(
            permit.key(),
            DispatchLedgerEntry::ReturnedNeverEntered {
                attempt_id: permit.attempt_id(),
            },
        );
        Ok(())
    }

    /// Retire a reserved permit as entry-uncertain when the caller cannot establish no entry.
    pub(super) fn retire_reserved_uncertain(
        &mut self,
        permit: DispatchPermit,
    ) -> Result<(), PermitFailure<DispatchPermit>> {
        if let Err(error) = self.validate_binding(&permit.binding) {
            return Err(PermitFailure::new(error, permit));
        }
        if let Err(error) = self.require_ledger_state(
            permit.key(),
            permit.attempt_id(),
            DispatchLedgerState::Reserved,
        ) {
            return Err(PermitFailure::new(error, permit));
        }
        self.ledger.insert(
            permit.key(),
            DispatchLedgerEntry::ReturnedUncertain {
                attempt_id: permit.attempt_id(),
            },
        );
        Ok(())
    }

    /// Request cancellation and close future issuance/entry atomically.
    pub(super) fn request_cancellation(
        &mut self,
        observation: CancellationRequestObservation,
    ) -> Result<(), DispatchOwnerError> {
        if self.lifecycle == DispatchOwnerLifecycle::Sealed {
            if self.progress.cancellation_request() == Some(observation) {
                return Ok(());
            }
            return Err(DispatchOwnerError::Progress(
                WorkProgressError::CancellationReplayConflict,
            ));
        }
        self.progress.request_cancellation(observation)?;
        self.lifecycle = DispatchOwnerLifecycle::CancelDraining;
        Ok(())
    }

    /// Retire an entered permit as unknown-effect after cancellation.
    pub(super) fn retire_entered_uncertain(
        &mut self,
        permit: EnteredDispatchPermit,
    ) -> Result<(), PermitFailure<EnteredDispatchPermit>> {
        if let Err(error) = self.require_cancellation_draining() {
            return Err(PermitFailure::new(error, permit));
        }
        if let Err(error) = self.validate_binding(&permit.binding) {
            return Err(PermitFailure::new(error, permit));
        }
        if let Err(error) = self.require_ledger_state(
            permit.key(),
            permit.attempt_id(),
            DispatchLedgerState::Entered,
        ) {
            return Err(PermitFailure::new(error, permit));
        }
        self.ledger.insert(
            permit.key(),
            DispatchLedgerEntry::ReturnedUncertain {
                attempt_id: permit.attempt_id(),
            },
        );
        Ok(())
    }

    /// Retire an entered permit with validated late evidence and retain its historical context.
    pub(super) fn retire_entered_completed(
        &mut self,
        permit: EnteredDispatchPermit,
        evidence: StoredStepEvidence,
    ) -> Result<CompletionContext, PermitFailure<EnteredDispatchPermit>> {
        if let Err(error) = self.require_cancellation_draining() {
            return Err(PermitFailure::new(error, permit));
        }
        if let Err(error) = self.validate_binding(&permit.binding) {
            return Err(PermitFailure::new(error, permit));
        }
        if let Err(error) = self.require_ledger_state(
            permit.key(),
            permit.attempt_id(),
            DispatchLedgerState::Entered,
        ) {
            return Err(PermitFailure::new(error, permit));
        }
        let context =
            CompletionContext::new(permit.binding.execution_epoch, permit.binding.scope.clone());
        if let Err(error) =
            self.progress
                .validate_historical_completion(permit.key(), &context, &evidence)
        {
            return Err(PermitFailure::new(
                DispatchOwnerError::Progress(error),
                permit,
            ));
        }
        self.ledger.insert(
            permit.key(),
            DispatchLedgerEntry::CompletedAfterCancellation {
                attempt_id: permit.attempt_id(),
                evidence,
                context: context.clone(),
            },
        );
        Ok(context)
    }

    /// Seal a private current-epoch barrier after every issued permit has an explicit outcome.
    pub(super) fn seal(&mut self) -> Result<VerifiedCancellationBarrier, DispatchOwnerError> {
        if self.lifecycle == DispatchOwnerLifecycle::Sealed {
            let dispatch_outcome = self
                .sealed_dispatch_outcome
                .as_ref()
                .ok_or(DispatchOwnerError::CancellationNotRequested)?
                .clone();
            return Ok(self.barrier(dispatch_outcome));
        }
        self.require_cancellation_draining()?;
        if self.progress.audit_observation().is_some()
            || self.progress.disposition() == FullProgressDisposition::AuditRequired
        {
            return Err(DispatchOwnerError::AuditActive);
        }
        let outstanding = self
            .ledger
            .values()
            .filter(|entry| {
                matches!(
                    entry,
                    DispatchLedgerEntry::Reserved { .. } | DispatchLedgerEntry::Entered { .. }
                )
            })
            .count();
        if outstanding != 0 {
            return Err(DispatchOwnerError::OutstandingPermits { count: outstanding });
        }
        let manifest = self.progress.cancellation_dispatch_manifest()?;
        let recovery_generation = self
            .progress
            .recovery_generation
            .checked_add(1)
            .ok_or(DispatchOwnerError::RecoveryGenerationExhausted)?;
        let rows = manifest
            .keys()
            .iter()
            .map(|key| {
                let outcome = match self.ledger.get(key) {
                    None | Some(DispatchLedgerEntry::ReturnedNeverEntered { .. }) => {
                        CancellationDispatchOutcome::NeverEntered
                    }
                    Some(DispatchLedgerEntry::ReturnedUncertain { .. }) => {
                        CancellationDispatchOutcome::EntryUncertain
                    }
                    Some(DispatchLedgerEntry::CompletedAfterCancellation {
                        evidence,
                        context,
                        ..
                    }) => CancellationDispatchOutcome::ObservedAfterCancellation {
                        evidence: evidence.clone(),
                        context: context.clone(),
                    },
                    Some(DispatchLedgerEntry::CompletedNormally { .. }) => {
                        return Err(DispatchOwnerError::NormalCompletionInManifest { key: *key });
                    }
                    Some(DispatchLedgerEntry::Reserved { .. })
                    | Some(DispatchLedgerEntry::Entered { .. }) => {
                        return Err(DispatchOwnerError::OutstandingPermits { count: 1 });
                    }
                };
                Ok(CancellationDispatchOutcomeRow::new(*key, outcome))
            })
            .collect::<Result<Vec<_>, DispatchOwnerError>>()?;
        let dispatch_outcome = CancellationDispatchOutcomeRecord::new(
            manifest,
            self.progress.execution_epoch,
            recovery_generation,
            rows,
        )?;
        self.progress
            .validate_cancellation_dispatch_record(&dispatch_outcome)?;
        self.sealed_dispatch_outcome = Some(dispatch_outcome.clone());
        self.lifecycle = DispatchOwnerLifecycle::Sealed;
        Ok(self.barrier(dispatch_outcome))
    }

    /// Accept the existing private after-effect reconciliation guard using this owner's sealed
    /// dispatch barrier.  Recovery and receipt lineage are derived from the owner progress so a
    /// caller cannot substitute an unrelated inventory or authority snapshot.
    pub(super) fn accept_after_effect_reconciliation(
        &mut self,
        barrier: &VerifiedCancellationBarrier,
        expected_recovery_generation: u64,
        source_closure_observation: CancellationSourceClosureObservationRecord,
        coordinator_closure_observation: CancellationCoordinatorClosureObservationRecord,
    ) -> Result<(), DispatchOwnerError> {
        barrier.validate_for_owner(self)?;
        let recovery_inventory = self
            .progress
            .cancellation_effect_recovery_inventory(barrier.dispatch_outcome())?;
        let authority = super::VerifiedAfterEffectReconciliation {
            operation_id: self.progress.operation_id,
            plan: self.progress.plan.clone(),
            execution_epoch: self.progress.execution_epoch,
            cancellation_inventory: barrier.cancellation_inventory().clone(),
            expected_recovery_generation,
            recovery_inventory,
            source_closure_observation,
            coordinator_closure_observation,
        };
        self.progress
            .accept_after_effect_reconciliation(authority)
            .map_err(DispatchOwnerError::Progress)
    }

    /// Accept existing nonterminal cleanup observations against this owner's sealed dispatch and
    /// current accepted after-effect state.  The owner supplies the exact after-effect snapshot;
    /// callers provide only the bounded resource and namespace observations and expected global
    /// generation.
    pub(super) fn accept_cancellation_cleanup_observations(
        &mut self,
        barrier: &VerifiedCancellationBarrier,
        expected_recovery_generation: u64,
        resource_cleanup_observation: CancellationResourceCleanupObservationRecord,
        namespace_cleanup_observation: CancellationNamespaceCleanupObservationRecord,
    ) -> Result<(), DispatchOwnerError> {
        barrier.validate_for_owner(self)?;
        let after_effect_reconciliation =
            self.progress.after_effect_reconciliation.as_ref().ok_or(
                DispatchOwnerError::Progress(
                    WorkProgressError::CancellationCleanupAfterEffectMissing,
                ),
            )?;
        if after_effect_reconciliation.execution_epoch() != self.progress.execution_epoch
            || after_effect_reconciliation
                .recovery_inventory()
                .dispatch_outcome()
                != barrier.dispatch_outcome()
        {
            return Err(DispatchOwnerError::AfterEffectDispatchMismatch);
        }
        let authority = super::VerifiedCancellationCleanupObservations {
            operation_id: self.progress.operation_id,
            plan: self.progress.plan.clone(),
            execution_epoch: self.progress.execution_epoch,
            cancellation_inventory: barrier.cancellation_inventory().clone(),
            expected_recovery_generation,
            after_effect_reconciliation: after_effect_reconciliation.clone(),
            resource_cleanup_observation,
            namespace_cleanup_observation,
        };
        self.progress
            .accept_cancellation_cleanup_observations(authority)
            .map_err(DispatchOwnerError::Progress)
    }

    /// Report one ledger state for focused owner tests without exposing mutable progress.
    pub(super) fn ledger_state(&self, key: WorkUnitKey) -> Option<DispatchLedgerState> {
        self.ledger.get(&key).map(DispatchLedgerEntry::state)
    }

    #[cfg(test)]
    pub(super) fn set_next_attempt_for_test(&mut self, next: u64) {
        self.next_attempt_id = next;
    }

    #[cfg(test)]
    pub(super) fn set_recovery_generation_for_test(&mut self, generation: u64) {
        self.progress.recovery_generation = generation;
    }

    fn barrier(
        &self,
        dispatch_outcome: CancellationDispatchOutcomeRecord,
    ) -> VerifiedCancellationBarrier {
        VerifiedCancellationBarrier {
            owner_id: self.owner_id,
            operation_id: self.progress.operation_id,
            plan: self.progress.plan.clone(),
            execution_epoch: self.progress.execution_epoch,
            cancellation_inventory: self
                .progress
                .cancellation_inventory
                .as_ref()
                .expect("sealed owner has cancellation inventory")
                .clone(),
            dispatch_outcome,
        }
    }

    fn require_cancellation_draining(&self) -> Result<(), DispatchOwnerError> {
        if self.lifecycle != DispatchOwnerLifecycle::CancelDraining {
            return Err(if self.lifecycle == DispatchOwnerLifecycle::Sealed {
                DispatchOwnerError::IssuanceClosed
            } else {
                DispatchOwnerError::LateRetirementBeforeCancellation
            });
        }
        Ok(())
    }

    fn validate_binding(&self, binding: &PermitBinding) -> Result<(), DispatchOwnerError> {
        if binding.owner_id != self.owner_id {
            return Err(DispatchOwnerError::PermitOwnerMismatch);
        }
        if binding.operation_id != self.progress.operation_id {
            return Err(DispatchOwnerError::PermitOperationMismatch);
        }
        if binding.execution_epoch != self.progress.execution_epoch {
            return Err(DispatchOwnerError::PermitEpochMismatch {
                expected: self.progress.execution_epoch,
                actual: binding.execution_epoch,
            });
        }
        Ok(())
    }

    fn require_ledger_state(
        &self,
        key: WorkUnitKey,
        attempt_id: u64,
        expected: DispatchLedgerState,
    ) -> Result<(), DispatchOwnerError> {
        let Some(entry) = self.ledger.get(&key) else {
            return Err(DispatchOwnerError::PermitNotTracked { key });
        };
        if entry.state() != expected || entry.attempt_id() != attempt_id {
            return Err(DispatchOwnerError::PermitStateMismatch { key });
        }
        Ok(())
    }
}
