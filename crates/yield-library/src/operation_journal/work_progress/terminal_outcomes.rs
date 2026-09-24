//! Bounded observations for the four guarded full-progress terminal outcomes.
//!
//! These values are deliberately separate from the private owner authorities in the parent
//! module.  A decoded observation can be checked against a frozen progress value, but it cannot
//! issue a compensation, release a claim, or create an escalation authority.

use serde::{Deserialize, Serialize};

use super::super::admission_claims::{AdmissionClaimId, AdmissionClaimSettlementInventory};
use super::super::cancellation_dispatch::CancellationDispatchOutcomeRecord;
use super::super::cancellation_effects::{
    CancellationEffectRecipe, CancellationEffectRecoveryInventory,
};
use super::super::coordinator::RetryObservation;
use super::super::participants::ParticipantId;
use super::{CompletionContext, FullProgressDisposition, OperationPhase, StoredStepEvidence};

/// Maximum number of rows retained by one terminal proof family.
pub(crate) const MAX_TERMINAL_OUTCOME_ROWS: usize = super::super::work_plan::MAX_WORK_PLAN_UNITS;

/// One of the four bounded terminal outcome observations.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) enum TerminalOutcomeObservation {
    /// A qualified compensating recovery completed.
    RolledBack(RolledBackObservation),
    /// A prepublication operation is blocked on an explicit user or capability decision.
    BlockedByUser(BlockedByUserObservation),
    /// Recovery exhausted its bounded attempts while retaining the affected evidence.
    FailedPreservingData(FailedPreservingDataObservation),
    /// Safety ambiguity remains unresolved and was escalated without destructive cleanup.
    FailedDataLossRisk(FailedDataLossRiskObservation),
}

/// Recipe-specific compensation receipt retained for one observed effect bundle.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompensationReceiptObservation {
    pub(crate) action_ordinal: u8,
    pub(crate) step_segment: u16,
    pub(crate) recipe: CancellationEffectRecipe,
    pub(crate) evidence_keys: Vec<super::WorkUnitKey>,
    pub(crate) results: Vec<CompensationResultObservation>,
}

/// Explicit outcome returned by a compensating owner for one original effect checkpoint.
///
/// This is intentionally distinct from [`StoredStepEvidence`].  Original effect evidence proves
/// what happened before rollback; this value records the separate compensating result and binds it
/// to the exact original key and recipe segment.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum CompensationResultKind {
    PublishedOutputRemoved,
    OriginRestored,
    RetainedStateRestored,
    ExternalStateReconciled,
}

/// Bounded compensation result tied to the current accepted participant context.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompensationResultObservation {
    pub(crate) outcome: CompensationResultKind,
    pub(crate) original_key: super::WorkUnitKey,
    pub(crate) action_ordinal: u8,
    pub(crate) step_segment: u16,
    pub(crate) context: CompletionContext,
    pub(crate) owner_revision: u64,
}

/// Proof that one origin-removal effect was compensated by restoring its origin.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OriginRestorationObservation {
    pub(crate) result: CompensationResultObservation,
}

/// Whether a compensation participant is source-owned or coordinator-owned.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum CompensationParticipantKind {
    Source,
    Coordinator,
}

/// Source/coordinator reconciliation receipt for a compensated outcome.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompensationReconciliationObservation {
    pub(crate) kind: CompensationParticipantKind,
    pub(crate) participant_id: ParticipantId,
    pub(crate) key: super::WorkUnitKey,
    pub(crate) context: CompletionContext,
    pub(crate) evidence: StoredStepEvidence,
    pub(crate) result: CompensationResultObservation,
}

/// Resource cleanup or retention observation tied to one exact admission claim.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum TerminalResourceOutcome {
    Absent,
    Retained,
}

/// One bounded resource observation used by rollback and failure terminal guards.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TerminalResourceObservation {
    pub(crate) claim_id: AdmissionClaimId,
    pub(crate) outcome: TerminalResourceOutcome,
    pub(crate) keys: Vec<super::WorkUnitKey>,
    pub(crate) owner_revision: u64,
    pub(crate) context: CompletionContext,
}

/// Whether dispatch was quiesced or retained for bounded recovery.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum TerminalDispatchQuiescenceOutcome {
    Quiesced,
    PreservedForRecovery,
}

/// Bounded dispatch quiescence evidence for a terminal outcome.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TerminalDispatchQuiescenceObservation {
    pub(crate) execution_epoch: u64,
    pub(crate) outcome: TerminalDispatchQuiescenceOutcome,
    pub(crate) keys: Vec<super::WorkUnitKey>,
}

/// Explicit user decision or capability action that blocks a prepublication operation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum BlockedByUserActionKind {
    UserDecision,
    CapabilityAction,
}

/// Participant-tied bounded blocked-by-user decision observation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BlockedByUserDecisionObservation {
    pub(crate) kind: BlockedByUserActionKind,
    pub(crate) participant_id: ParticipantId,
    pub(crate) key: super::WorkUnitKey,
    pub(crate) context: CompletionContext,
}

/// Bounded retry-budget exhaustion receipt tied to retained retry history.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecoveryExhaustionObservation {
    pub(crate) attempt_budget: u32,
    pub(crate) exhausted_attempt: u32,
    pub(crate) last_retry: RetryObservation,
    pub(crate) failed_participants: Vec<FailedParticipantObservation>,
}

/// One participant retained in the failed retry history.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FailedParticipantObservation {
    pub(crate) key: super::WorkUnitKey,
    pub(crate) attempt: u32,
    pub(crate) context: CompletionContext,
    pub(crate) evidence: StoredStepEvidence,
}

/// Bounded escalation reason for unresolved safety ambiguity.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum SafetyEscalationReason {
    OriginAbsenceUnverified,
    CapabilityIdentityAmbiguous,
    DispatchEntryUncertain,
    ReconciliationUnresolved,
}

/// Existing-audit-linked escalation receipt for data-loss risk.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SafetyEscalationObservation {
    pub(crate) reason: SafetyEscalationReason,
    pub(crate) audit_generation: u64,
    pub(crate) participant_key: super::WorkUnitKey,
    pub(crate) context: CompletionContext,
    pub(crate) evidence: StoredStepEvidence,
}

/// Complete bounded rollback observation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RolledBackObservation {
    pub(crate) execution_epoch: u64,
    pub(crate) recovery_generation: u64,
    pub(crate) prior_phase: OperationPhase,
    pub(crate) prior_disposition: FullProgressDisposition,
    pub(crate) effect_inventory: CancellationEffectRecoveryInventory,
    pub(crate) compensation_receipts: Vec<CompensationReceiptObservation>,
    pub(crate) origin_restorations: Vec<OriginRestorationObservation>,
    pub(crate) reconciliations: Vec<CompensationReconciliationObservation>,
    pub(crate) resource_observations: Vec<TerminalResourceObservation>,
    pub(crate) claim_settlement: AdmissionClaimSettlementInventory,
    pub(crate) dispatch_quiescence: TerminalDispatchQuiescenceObservation,
}

/// Complete bounded prepublication blocked-by-user observation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BlockedByUserObservation {
    pub(crate) execution_epoch: u64,
    pub(crate) recovery_generation: u64,
    pub(crate) prior_phase: OperationPhase,
    pub(crate) prior_disposition: FullProgressDisposition,
    pub(crate) decision: BlockedByUserDecisionObservation,
    pub(crate) cleanup: super::PrePublishCleanupObservation,
    pub(crate) unstarted_quiescence: super::PrePublishUnstartedQuiescenceObservation,
    pub(crate) settlement: AdmissionClaimSettlementInventory,
    pub(crate) dispatch_outcome: CancellationDispatchOutcomeRecord,
    pub(crate) dispatch_quiescence: TerminalDispatchQuiescenceObservation,
}

/// Complete bounded preserved-data failure observation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FailedPreservingDataObservation {
    pub(crate) execution_epoch: u64,
    pub(crate) recovery_generation: u64,
    pub(crate) prior_phase: OperationPhase,
    pub(crate) prior_disposition: FullProgressDisposition,
    pub(crate) exhaustion: RecoveryExhaustionObservation,
    pub(crate) resource_observations: Vec<TerminalResourceObservation>,
    pub(crate) claim_settlement: AdmissionClaimSettlementInventory,
    pub(crate) dispatch_quiescence: TerminalDispatchQuiescenceObservation,
}

/// Complete bounded data-loss-risk failure observation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FailedDataLossRiskObservation {
    pub(crate) execution_epoch: u64,
    pub(crate) recovery_generation: u64,
    pub(crate) prior_phase: OperationPhase,
    pub(crate) prior_disposition: FullProgressDisposition,
    pub(crate) escalation: SafetyEscalationObservation,
    pub(crate) resource_observations: Vec<TerminalResourceObservation>,
    pub(crate) claim_settlement: AdmissionClaimSettlementInventory,
    pub(crate) dispatch_quiescence: TerminalDispatchQuiescenceObservation,
}
