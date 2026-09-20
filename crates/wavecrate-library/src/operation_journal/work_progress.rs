//! Pure guarded progress over a frozen [`super::work_plan::WorkPlan`].
//!
//! This module keeps the coordinator's bounded state separate from the legacy journal schemas.
//! Initial step states are derived from the complete V1 plan; callers cannot supply a sparse
//! completion list or choose which staged step is inapplicable.  Filesystem, source, and
//! coordinator values below are observations only.  A completion changes progress only when a
//! crate-private, non-serialized [`VerifiedStepCompletion`] binds the operation, exact plan,
//! participant scope, role, and evidence together.  This module has no production issuer for
//! that authority and performs no filesystem, SQLite, capacity, or owner work.
//!
//! Relative locators and target identities remain in the immutable command and frozen plan.  The
//! progress model adds no absolute path or pathname identity.  Source observations are checked
//! against the captured source fence and may retain that revision for a no-op audit or advance it
//! for an owner commit; they may never move backwards or change source, lifecycle, root, database,
//! or lease identity.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

use super::admission_claims::{
    AdmissionClaimId, AdmissionClaimInventory, AdmissionClaimSettlementInventory,
    AdmissionClaimSettlementOutcome, AdmissionClaimValidationError,
};
use super::cancellation_cleanup::{
    CancellationNamespaceCleanupManifest, CancellationNamespaceCleanupManifestError,
    CancellationNamespaceCleanupObservationRecord,
    CancellationNamespaceCleanupObservationRecordError, CancellationNamespaceCleanupObservationRow,
    CancellationNamespaceCleanupOutcome, CancellationResourceCleanupManifest,
    CancellationResourceCleanupManifestError, CancellationResourceCleanupObservationRecord,
    CancellationResourceCleanupObservationRecordError, CancellationResourceCleanupObservationRow,
    CancellationResourceCleanupOutcome,
};
use super::cancellation_dispatch::{
    CancellationDispatchManifest, CancellationDispatchManifestError, CancellationDispatchOutcome,
    CancellationDispatchOutcomeRecord, CancellationDispatchOutcomeRecordError,
    derive_cancellation_dispatch_manifest as derive_dispatch_manifest,
};
use super::cancellation_effects::{
    CancellationCleanupManifest, CancellationCleanupManifestError, CancellationEffectInventory,
    CancellationEffectInventoryError, CancellationEffectRecoveryInventory,
    derive_cancellation_cleanup_manifest,
    derive_cancellation_effect_inventory as derive_effect_inventory,
    derive_cancellation_effect_recovery_inventory as derive_effect_recovery_inventory,
};
use super::cancellation_reconciliation::{
    CancellationCoordinatorClosureObservationRecord,
    CancellationCoordinatorClosureObservationRecordError, CancellationCoordinatorClosureOutcome,
    CancellationSourceClosureObservationRecord, CancellationSourceClosureObservationRecordError,
    CancellationSourceClosureOutcome, CoordinatorClosureManifest, CoordinatorClosureManifestError,
    SourceClosureManifest, SourceClosureManifestError,
};
use super::cancellation_recovery::{
    CancellationActionState, CancellationInventory, CancellationInventoryError, CancellationRoute,
    MAX_CANCELLATION_ACTIONS,
};
use super::coordinator::{RetryObservation, RetryObservationValidationError, UniversalCommandArgs};
use super::full_capacity::FullCapacityAllocationClass;
use super::full_status::FullProgressDisposition;
use super::model::{
    CancellationRequestObservation, CancellationRequestValidationError, OperationId, OperationPhase,
};
use super::namespace_commands::NamespaceCommandArgs;
use super::participants::{
    MAX_SOURCE_IDENTITY_BYTES, ParticipantDescriptor, ParticipantId, ParticipantKind,
    ParticipantRequirement, ParticipantSet, ParticipantTextField, ParticipantTextViolation,
    ParticipantValidationError, StoredSourceFence,
};
use super::readiness_scheduling::{
    ReadinessDeferralBatchObservation, ReadinessDeferralManifest, ReadinessDeferralManifestError,
    ReadinessSchedulingObservationError, ReadinessSchedulingObservationRecord,
    ReadinessSchedulingObservationRow, derive_readiness_deferral_manifest,
};
use super::work_plan::{
    GLOBAL_ACTION_ORDINAL, MAX_WORK_PLAN_UNITS, WorkPlan, WorkPlanError, WorkPrerequisite,
    WorkUnitApplicability, WorkUnitKey, WorkUnitRole,
};
use crate::sample_sources::SourceId;

/// Private in-memory work-unit dispatch ownership.  The module has no production constructor;
/// admission/provider integration must issue a future owner after establishing real ownership.
#[allow(dead_code)]
#[path = "work_progress/dispatch_owner.rs"]
pub(super) mod dispatch_owner;

/// Bounded, non-authoritative full-progress snapshot observations.
#[path = "work_progress/snapshot.rs"]
pub(super) mod snapshot;

pub use snapshot::{FullProgressSnapshotObservation, FullProgressSnapshotObservationError};

/// One bounded source revision observation attached to a progress completion.
///
/// The source identity, root and database identities, lifecycle generation, and lease epoch are
/// captured observations.  They do not open or retain a source lease.  `revision` may equal the
/// initial fence for a no-op audit or exceed it after an owner commit.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRevisionObservation {
    source_id: SourceId,
    root_identity: String,
    database_identity: String,
    lifecycle_generation: u64,
    lease_epoch: u64,
    revision: u64,
}

impl SourceRevisionObservation {
    /// Construct a bounded source revision observation without acquiring a lease.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        source_id: SourceId,
        root_identity: impl AsRef<str>,
        database_identity: impl AsRef<str>,
        lifecycle_generation: u64,
        lease_epoch: u64,
        revision: u64,
    ) -> Result<Self, ProgressEvidenceError> {
        let root_identity = root_identity.as_ref();
        let database_identity = database_identity.as_ref();
        validate_source_text(
            source_id.as_str(),
            ParticipantTextField::SourceId,
            MAX_SOURCE_IDENTITY_BYTES,
        )?;
        validate_source_text(
            root_identity,
            ParticipantTextField::RootIdentity,
            MAX_SOURCE_IDENTITY_BYTES,
        )?;
        validate_source_text(
            database_identity,
            ParticipantTextField::DatabaseIdentity,
            MAX_SOURCE_IDENTITY_BYTES,
        )?;
        if lease_epoch == 0 {
            return Err(ProgressEvidenceError::InvalidSourceObservation(
                ParticipantValidationError::InvalidLeaseEpoch,
            ));
        }
        let observation = Self {
            source_id,
            root_identity: root_identity.to_owned(),
            database_identity: database_identity.to_owned(),
            lifecycle_generation,
            lease_epoch,
            revision,
        };
        observation.validate()?;
        Ok(observation)
    }

    /// Validate the bounded source identity fields.
    pub fn validate(&self) -> Result<(), ProgressEvidenceError> {
        validate_source_text(
            self.source_id.as_str(),
            ParticipantTextField::SourceId,
            MAX_SOURCE_IDENTITY_BYTES,
        )?;
        validate_source_text(
            &self.root_identity,
            ParticipantTextField::RootIdentity,
            MAX_SOURCE_IDENTITY_BYTES,
        )?;
        validate_source_text(
            &self.database_identity,
            ParticipantTextField::DatabaseIdentity,
            MAX_SOURCE_IDENTITY_BYTES,
        )?;
        if self.lease_epoch == 0 {
            return Err(ProgressEvidenceError::InvalidSourceObservation(
                ParticipantValidationError::InvalidLeaseEpoch,
            ));
        }
        Ok(())
    }

    /// Return the observed source identity.
    pub fn source_id(&self) -> &SourceId {
        &self.source_id
    }

    /// Return the observed source-root identity.
    pub fn root_identity(&self) -> &str {
        &self.root_identity
    }

    /// Return the observed source-database identity.
    pub fn database_identity(&self) -> &str {
        &self.database_identity
    }

    /// Return the observed source lifecycle generation.
    pub const fn lifecycle_generation(&self) -> u64 {
        self.lifecycle_generation
    }

    /// Return the observed source lease epoch.
    pub const fn lease_epoch(&self) -> u64 {
        self.lease_epoch
    }

    /// Return the observed committed or audited source revision.
    pub const fn revision(&self) -> u64 {
        self.revision
    }
}

/// Closed filesystem receipt categories that can be attached to a matching work role.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum FilesystemReceiptKind {
    /// Preparation and capability-relative planning was recorded by the owner.
    Prepared,
    /// Recovery material was retained before a destructive edit.
    RecoveryMaterialRetained,
    /// A durable restore record was prepared before a retention relocation.
    RestoreRecordPrepared,
    /// Contained destination parents were prepared.
    EnsureContainedParentDirectories,
    /// A destination-local staging object was created.
    Stage,
    /// A staged object was copied and validated.
    CopyValidated,
    /// A final publication was claimed and verified.
    Publish,
    /// A qualified direct relocation was claimed and verified.
    QualifiedRelocation,
    /// A retained payload was verified after relocation.
    RetainedPayloadVerified,
    /// A selected retained object disposition was applied.
    SelectedRetainedObjectDisposition,
    /// A selected retained object was verified absent.
    RetainedObjectAbsence,
    /// Durable origin-removal intent was recorded.
    OriginRemovalStart,
    /// The origin was verified absent.
    OriginAbsence,
    /// A bounded retained merge was applied.
    RetainedMerge,
    /// An external filesystem observation was audited.
    ExternalAudit,
}

/// Bounded filesystem receipt summary retained as non-authoritative evidence.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FilesystemReceiptSummary {
    kind: FilesystemReceiptKind,
    owner_revision: u64,
}

impl FilesystemReceiptSummary {
    /// Construct a role-specific filesystem receipt observation.
    pub const fn new(kind: FilesystemReceiptKind, owner_revision: u64) -> Self {
        Self {
            kind,
            owner_revision,
        }
    }

    /// Return the closed receipt category.
    pub const fn kind(self) -> FilesystemReceiptKind {
        self.kind
    }

    /// Return the owner's bounded receipt revision observation.
    pub const fn owner_revision(self) -> u64 {
        self.owner_revision
    }
}

/// Closed coordinator receipt categories that can be attached to a matching work role.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum CoordinatorReceiptKind {
    /// Profile-global library reconciliation.
    GlobalLibrary,
    /// Metadata harvest reconciliation.
    Harvest,
    /// Browser or folder projection publication.
    Projection,
    /// Readiness scheduling.
    Readiness,
    /// One artifact rebuild or verification.
    Artifact,
}

/// A bounded unresolved coordinator deficit observation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum CoordinatorDeficit {
    /// A required participant remains incomplete.
    RequiredParticipant(ParticipantId),
    /// A source projection remains behind its committed revision.
    ProjectionGap(ParticipantId),
    /// An optional rebuildable artifact remains deferred.
    OptionalArtifact(ParticipantId),
}

/// Bounded coordinator receipt and deficit observation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CoordinatorReceiptSummary {
    kind: CoordinatorReceiptKind,
    coordinator_revision: u64,
    deficit: Option<CoordinatorDeficit>,
    source_observation: Option<SourceRevisionObservation>,
}

impl CoordinatorReceiptSummary {
    /// Construct a coordinator receipt observation without granting transition authority.
    pub const fn new(
        kind: CoordinatorReceiptKind,
        coordinator_revision: u64,
        deficit: Option<CoordinatorDeficit>,
    ) -> Self {
        Self {
            kind,
            coordinator_revision,
            deficit,
            source_observation: None,
        }
    }

    /// Attach the source-fence revision observed by a source-scoped coordinator owner.
    pub fn with_source_observation(mut self, observation: SourceRevisionObservation) -> Self {
        self.source_observation = Some(observation);
        self
    }

    /// Validate the optional source observation retained by this receipt.
    pub fn validate(&self) -> Result<(), ProgressEvidenceError> {
        if let Some(observation) = &self.source_observation {
            observation.validate()?
        }
        Ok(())
    }

    /// Return the closed coordinator receipt category.
    pub const fn kind(&self) -> CoordinatorReceiptKind {
        self.kind
    }

    /// Return the coordinator revision observation.
    pub const fn coordinator_revision(&self) -> u64 {
        self.coordinator_revision
    }

    /// Return an unresolved bounded deficit, when one was observed.
    pub const fn deficit(&self) -> Option<CoordinatorDeficit> {
        self.deficit
    }

    /// Borrow the source-fence revision observation, when this receipt is source-scoped.
    pub fn source_observation(&self) -> Option<&SourceRevisionObservation> {
        self.source_observation.as_ref()
    }
}

/// Non-authoritative bounded evidence for one completed work role.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum StoredStepEvidence {
    /// Source data was observed at the retained source fence or a later revision.
    SourceObserved {
        /// Source identity and revision observation.
        observation: SourceRevisionObservation,
    },
    /// A source writer committed a revision at or after the retained source fence.
    SourceCommitted {
        /// Revision observed immediately before the source commit.
        before_revision: u64,
        /// Source identity and revision observed after the source commit.
        after: SourceRevisionObservation,
    },
    /// A filesystem owner returned a role-specific receipt summary.
    Filesystem {
        /// Filesystem receipt observation.
        receipt: FilesystemReceiptSummary,
    },
    /// A global coordinator owner returned a role-specific receipt summary.
    Coordinator {
        /// Coordinator receipt observation.
        receipt: CoordinatorReceiptSummary,
    },
    /// A normal Readiness owner returned the bounded scheduling observation for this completion.
    ///
    /// This remains an untrusted receipt.  It carries the scheduling row needed to validate
    /// normal Readiness history, but does not prove that a durable wake was persisted or grant
    /// permission to defer an Artifact.
    ReadinessScheduled {
        /// Readiness scheduling observation bound to the completed Readiness unit.
        observation: ReadinessSchedulingObservationRow,
    },
}

impl StoredStepEvidence {
    /// Validate nested evidence shape without treating it as completion authority.
    pub fn validate(&self) -> Result<(), ProgressEvidenceError> {
        match self {
            Self::SourceObserved { observation } => observation.validate(),
            Self::SourceCommitted { after, .. } => after.validate(),
            Self::Filesystem { .. } => Ok(()),
            Self::Coordinator { receipt } => receipt.validate(),
            Self::ReadinessScheduled { observation } => observation
                .validate_for_stored_step_evidence()
                .map_err(|error| {
                    ProgressEvidenceError::ReadinessSchedulingObservation(Box::new(error))
                }),
        }
    }
}

/// Closed reason for a guarded audit disposition.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ProgressAuditReason {
    /// Evidence could not establish one affected identity or boundary.
    UncertainEvidence,
    /// A source or projection revision relationship was not established.
    RevisionGap,
    /// A late final-name collision requires inspection.
    LateCollision,
    /// Watcher continuity was not proven.
    WatcherContinuityUnproven,
}

/// Bounded target set for one audit overlay.
///
/// `Steps` must be sorted by the stable [`WorkUnitKey`] order, contain no duplicate, and contain
/// at least one key.  The plan membership check is performed when the target is attached to a
/// progress record; a target value by itself is only a bounded observation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum AuditTarget {
    /// Audit the complete operation while preserving every existing step observation.
    WholeOperation,
    /// Audit this exact sorted, unique subset of frozen work-unit keys.
    Steps(Vec<WorkUnitKey>),
}

impl AuditTarget {
    /// Construct and shape-check a step target without checking plan membership.
    pub fn steps(keys: Vec<WorkUnitKey>) -> Result<Self, AuditTargetValidationError> {
        let target = Self::Steps(keys);
        target.validate_shape()?;
        Ok(target)
    }

    fn validate_shape(&self) -> Result<(), AuditTargetValidationError> {
        let Self::Steps(keys) = self else {
            return Ok(());
        };
        if keys.is_empty() {
            return Err(AuditTargetValidationError::Empty);
        }
        if keys.len() > MAX_WORK_PLAN_UNITS {
            return Err(AuditTargetValidationError::TooMany {
                actual: keys.len(),
                maximum: MAX_WORK_PLAN_UNITS,
            });
        }
        if keys.windows(2).any(|window| window[0] >= window[1]) {
            return Err(AuditTargetValidationError::NotSortedUnique);
        }
        Ok(())
    }

    fn validate_for_plan(&self, plan: &WorkPlan) -> Result<(), AuditTargetValidationError> {
        self.validate_shape()?;
        let Self::Steps(keys) = self else {
            return Ok(());
        };
        for key in keys {
            if !plan.units().iter().any(|unit| unit.key() == *key) {
                return Err(AuditTargetValidationError::UnknownWorkUnit { key: *key });
            }
        }
        Ok(())
    }
}

/// Failure while validating a bounded audit target.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum AuditTargetValidationError {
    /// A step target contained no keys.
    #[error("audit target must contain at least one work-unit key")]
    Empty,
    /// A step target exceeded the frozen-plan unit bound.
    #[error("audit target contains {actual} keys, maximum is {maximum}")]
    TooMany {
        /// Number of supplied target keys.
        actual: usize,
        /// Maximum number of target keys.
        maximum: usize,
    },
    /// A step target was not strictly sorted or contained a duplicate.
    #[error("audit target keys must be sorted and unique")]
    NotSortedUnique,
    /// A step target named a key absent from the frozen plan.
    #[error("audit target names unknown work unit {key:?}")]
    UnknownWorkUnit {
        /// Unknown key named by the target.
        key: WorkUnitKey,
    },
}

/// Bounded audit observation retained when progress must stop and preserve evidence.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressAuditObservation {
    reason: ProgressAuditReason,
    target: AuditTarget,
}

impl ProgressAuditObservation {
    /// Construct an audit observation without resolving the audit.
    pub const fn new(reason: ProgressAuditReason) -> Self {
        Self {
            reason,
            target: AuditTarget::WholeOperation,
        }
    }

    /// Construct an audit observation for a bounded subset of frozen work units.
    pub fn for_target(
        reason: ProgressAuditReason,
        target: AuditTarget,
    ) -> Result<Self, AuditTargetValidationError> {
        target.validate_shape()?;
        Ok(Self { reason, target })
    }

    fn validate_for_plan(&self, plan: &WorkPlan) -> Result<(), AuditTargetValidationError> {
        self.target.validate_for_plan(plan)
    }

    /// Return the closed audit reason.
    pub const fn reason(&self) -> ProgressAuditReason {
        self.reason
    }

    /// Borrow the exact audit target.
    pub fn target(&self) -> &AuditTarget {
        &self.target
    }
}

/// The progress disposition hidden beneath an active audit overlay.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum AuditSuspendedHold {
    /// No retry or cancellation disposition was active when the audit began.
    None,
    /// A retry observation remains pending beneath the audit.
    RetryPending,
    /// Cancellation was requested before publication beneath the audit.
    CancelRequestedBeforePublish,
    /// Cancellation was requested after publication beneath the audit.
    CancelRequestedAfterPublish,
    /// Cancellation was requested after an observed non-publication effect beneath the audit.
    CancelRequestedAfterObservedEffect,
}

/// Current audit overlay with its exact generation and suspended disposition.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ActiveAudit {
    generation: u64,
    observation: ProgressAuditObservation,
    suspended_hold: AuditSuspendedHold,
}

/// Immutable participant scope captured from the initial participant inventory.
///
/// A scope is an observation of participant kind and source fence.  It is not a lease, owner
/// capability, filesystem root, or permission to perform the role.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ParticipantScope {
    participant_id: ParticipantId,
    kind: ParticipantKind,
    source_fence: Option<StoredSourceFence>,
}

impl ParticipantScope {
    fn from_descriptor(descriptor: &ParticipantDescriptor) -> Self {
        Self {
            participant_id: descriptor.id(),
            kind: descriptor.kind(),
            source_fence: descriptor.source_fence().cloned(),
        }
    }

    /// Return the scoped participant identity.
    pub const fn participant_id(&self) -> ParticipantId {
        self.participant_id
    }

    /// Return the scoped participant family.
    pub const fn kind(&self) -> ParticipantKind {
        self.kind
    }

    /// Borrow the captured source fence, when the participant is source-scoped.
    pub fn source_fence(&self) -> Option<&StoredSourceFence> {
        self.source_fence.as_ref()
    }
}

/// Immutable context accepted with one completed step.
///
/// The context is retained only in memory so a completion can be replayed after source
/// reacquisition against the historical epoch and participant fence that accepted it.  It is an
/// observation, not an owner capability or completion authority.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionContext {
    execution_epoch: u64,
    scope: ParticipantScope,
}

impl CompletionContext {
    pub(crate) fn new(execution_epoch: u64, scope: ParticipantScope) -> Self {
        Self {
            execution_epoch,
            scope,
        }
    }

    /// Return the execution epoch accepted with the completion.
    pub const fn execution_epoch(&self) -> u64 {
        self.execution_epoch
    }

    /// Borrow the full participant scope accepted with the completion.
    pub fn scope(&self) -> &ParticipantScope {
        &self.scope
    }
}

/// Per-key progress status derived from the frozen plan.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ProgressStepStatus {
    /// The applicable work has not completed.
    Pending,
    /// A verified completion was accepted and its observation was retained.
    Completed,
    /// The frozen plan declared this step inapplicable.
    NotApplicable,
    /// Optional rebuildable artifact work was deliberately deferred.
    Deferred,
}

/// One exact work-plan key and its guarded progress status.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkProgressStep {
    key: WorkUnitKey,
    requirement: ParticipantRequirement,
    status: ProgressStepStatus,
    evidence: Option<StoredStepEvidence>,
    completion_context: Option<CompletionContext>,
}

/// Bounded observation of the final-claim state during prepublication cleanup.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum PrePublishFinalClaimObservation {
    /// The owner established that no application publication claim was ever entered.
    NoFinalClaim,
    /// The owner could not establish whether a final application claim remains.
    Uncertain,
}

/// Bounded observation of the action's qualified input/origin state during cleanup.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum PrePublishOriginalStateObservation {
    /// The action's qualified input/origin state remains preserved.
    ///
    /// For retained restore and purge actions, this refers to the selected retained payload.  It
    /// does not assert that the original source-relative pathname exists.
    Preserved,
    /// The action was a creation and therefore has no original object.
    NoOriginalObject,
    /// The owner could not establish the required input/origin state.
    Uncertain,
}

/// Bounded observation that one cleanup namespace is absent.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum PrePublishAbsenceObservation {
    /// The selected cleanup namespace is absent.
    Absent,
    /// The selected cleanup namespace is known to remain present and retained.
    Preserved,
    /// The owner could not establish absence.
    Uncertain,
}

/// Maximum number of unstarted-action quiescence rows retained for one cancellation.
pub const MAX_PRE_PUBLISH_UNSTARTED_QUIESCENCE_ACTIONS: usize = MAX_CANCELLATION_ACTIONS;
/// Maximum number of eligible pending work-unit keys retained by one quiescence observation.
pub const MAX_PRE_PUBLISH_UNSTARTED_QUIESCENCE_KEYS: usize = MAX_WORK_PLAN_UNITS;

/// Result of an owner barrier for an action that remained `NotStarted` in the frozen inventory.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum PrePublishUnstartedQuiescenceResult {
    /// A verified barrier established that the eligible work was never entered.
    QuiescedWithoutEntry,
    /// The owner could not establish the no-entry boundary.
    Uncertain,
}

/// One bounded quiescence observation for a frozen `NotStarted` action.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PrePublishUnstartedQuiescenceAction {
    action_ordinal: u8,
    eligible_pending_keys: Vec<WorkUnitKey>,
    result: PrePublishUnstartedQuiescenceResult,
    staging_absence: Option<PrePublishAbsenceObservation>,
    operation_recovery_payload_absence: Option<PrePublishAbsenceObservation>,
}

impl PrePublishUnstartedQuiescenceAction {
    /// Construct one bounded per-action quiescence observation.
    pub fn new(
        action_ordinal: u8,
        eligible_pending_keys: Vec<WorkUnitKey>,
        result: PrePublishUnstartedQuiescenceResult,
        staging_absence: Option<PrePublishAbsenceObservation>,
        operation_recovery_payload_absence: Option<PrePublishAbsenceObservation>,
    ) -> Self {
        Self {
            action_ordinal,
            eligible_pending_keys,
            result,
            staging_absence,
            operation_recovery_payload_absence,
        }
    }

    /// Return the frozen action ordinal covered by this observation.
    pub const fn action_ordinal(&self) -> u8 {
        self.action_ordinal
    }

    /// Borrow the exact sorted eligible pending keys required for this boundary.
    pub fn eligible_pending_keys(&self) -> &[WorkUnitKey] {
        &self.eligible_pending_keys
    }

    /// Return the owner barrier result.
    pub const fn result(&self) -> PrePublishUnstartedQuiescenceResult {
        self.result
    }

    /// Return the optional staging absence observation.
    pub const fn staging_absence(&self) -> Option<PrePublishAbsenceObservation> {
        self.staging_absence
    }

    /// Return the optional operation-recovery-payload absence observation.
    pub const fn operation_recovery_payload_absence(&self) -> Option<PrePublishAbsenceObservation> {
        self.operation_recovery_payload_absence
    }
}

/// Bounded ordered quiescence observations for unstarted prepublication actions.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PrePublishUnstartedQuiescenceObservation {
    actions: Vec<PrePublishUnstartedQuiescenceAction>,
}

impl PrePublishUnstartedQuiescenceObservation {
    /// Construct ordered quiescence observations after checking their local bounded shape.
    pub fn new(
        actions: Vec<PrePublishUnstartedQuiescenceAction>,
    ) -> Result<Self, PrePublishUnstartedQuiescenceValidationError> {
        let observation = Self { actions };
        observation.validate()?;
        Ok(observation)
    }

    /// Validate bounded action and key ordering without interpreting owner authority.
    pub fn validate(&self) -> Result<(), PrePublishUnstartedQuiescenceValidationError> {
        if self.actions.len() > MAX_PRE_PUBLISH_UNSTARTED_QUIESCENCE_ACTIONS {
            return Err(
                PrePublishUnstartedQuiescenceValidationError::TooManyActions {
                    actual: self.actions.len(),
                    maximum: MAX_PRE_PUBLISH_UNSTARTED_QUIESCENCE_ACTIONS,
                },
            );
        }
        let mut key_count = 0usize;
        for action in &self.actions {
            key_count = key_count.saturating_add(action.eligible_pending_keys.len());
            if key_count > MAX_PRE_PUBLISH_UNSTARTED_QUIESCENCE_KEYS {
                return Err(PrePublishUnstartedQuiescenceValidationError::TooManyKeys {
                    actual: key_count,
                    maximum: MAX_PRE_PUBLISH_UNSTARTED_QUIESCENCE_KEYS,
                });
            }
        }
        for (index, action) in self.actions.iter().enumerate() {
            if index > 0 && self.actions[index - 1].action_ordinal >= action.action_ordinal {
                return Err(
                    PrePublishUnstartedQuiescenceValidationError::ActionsNotSortedOrUnique {
                        index,
                        action_ordinal: action.action_ordinal,
                    },
                );
            }
            for (key_index, key) in action.eligible_pending_keys.iter().enumerate() {
                if key_index > 0 && action.eligible_pending_keys[key_index - 1] >= *key {
                    return Err(
                        PrePublishUnstartedQuiescenceValidationError::KeysNotSortedOrUnique {
                            action_ordinal: action.action_ordinal,
                            index: key_index,
                        },
                    );
                }
            }
        }
        Ok(())
    }

    /// Borrow quiescence observations in increasing action order.
    pub fn actions(&self) -> &[PrePublishUnstartedQuiescenceAction] {
        &self.actions
    }
}

/// Per-action cleanup observations retained by a prepublication cancellation outcome.
///
/// Missing fields and `Uncertain` values describe incomplete observations.  They are retained for
/// later owner work but cannot by themselves authorize a terminal cancellation.  Parent and
/// restore-record preparation are intentionally not deletion obligations in this shape.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PrePublishActionCleanup {
    action_ordinal: u8,
    final_claim: Option<PrePublishFinalClaimObservation>,
    original_state: Option<PrePublishOriginalStateObservation>,
    staging_absence: Option<PrePublishAbsenceObservation>,
    operation_recovery_payload_absence: Option<PrePublishAbsenceObservation>,
}

impl PrePublishActionCleanup {
    /// Construct one bounded per-action cleanup observation.
    pub fn new(
        action_ordinal: u8,
        final_claim: Option<PrePublishFinalClaimObservation>,
        original_state: Option<PrePublishOriginalStateObservation>,
        staging_absence: Option<PrePublishAbsenceObservation>,
        operation_recovery_payload_absence: Option<PrePublishAbsenceObservation>,
    ) -> Self {
        Self {
            action_ordinal,
            final_claim,
            original_state,
            staging_absence,
            operation_recovery_payload_absence,
        }
    }

    /// Return the action ordinal covered by this observation.
    pub const fn action_ordinal(&self) -> u8 {
        self.action_ordinal
    }

    /// Return the optional final-claim observation.
    pub const fn final_claim(&self) -> Option<PrePublishFinalClaimObservation> {
        self.final_claim
    }

    /// Return the optional qualified input/origin observation.
    pub const fn original_state(&self) -> Option<PrePublishOriginalStateObservation> {
        self.original_state
    }

    /// Return the optional staging-namespace absence observation.
    pub const fn staging_absence(&self) -> Option<PrePublishAbsenceObservation> {
        self.staging_absence
    }

    /// Return the optional operation-recovery-payload absence observation.
    pub const fn operation_recovery_payload_absence(&self) -> Option<PrePublishAbsenceObservation> {
        self.operation_recovery_payload_absence
    }
}

/// Bounded ordered cleanup observations for a prepublication cancellation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PrePublishCleanupObservation {
    actions: Vec<PrePublishActionCleanup>,
}

impl PrePublishCleanupObservation {
    /// Construct ordered cleanup observations after checking their local bounded shape.
    pub fn new(
        actions: Vec<PrePublishActionCleanup>,
    ) -> Result<Self, PrePublishCleanupValidationError> {
        let observation = Self { actions };
        observation.validate()?;
        Ok(observation)
    }

    /// Validate the bounded ordered action list without interpreting owner authority.
    pub fn validate(&self) -> Result<(), PrePublishCleanupValidationError> {
        if self.actions.len() > MAX_CANCELLATION_ACTIONS {
            return Err(PrePublishCleanupValidationError::TooManyActions {
                actual: self.actions.len(),
                maximum: MAX_CANCELLATION_ACTIONS,
            });
        }
        for (index, action) in self.actions.iter().enumerate() {
            if index > 0 && self.actions[index - 1].action_ordinal >= action.action_ordinal {
                return Err(PrePublishCleanupValidationError::ActionsNotSortedOrUnique {
                    index,
                    action_ordinal: action.action_ordinal,
                });
            }
        }
        Ok(())
    }

    /// Borrow cleanup observations in increasing action order.
    pub fn actions(&self) -> &[PrePublishActionCleanup] {
        &self.actions
    }
}

/// Stored prepublication cancellation observations, including cleanup and claim settlement.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PrePublishCancellationObservation {
    cleanup: PrePublishCleanupObservation,
    unstarted_quiescence: PrePublishUnstartedQuiescenceObservation,
    settlement: AdmissionClaimSettlementInventory,
    /// The accepted dispatch observation is retained for owner replay when present.  Older
    /// observational values may omit this optional field; decoding one never creates the
    /// private authority required to finish cancellation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dispatch_outcome: Option<CancellationDispatchOutcomeRecord>,
}

/// Immutable accepted cancellation-local after-effect reconciliation state.
///
/// This value retains the complete refined effect inventory and both cancellation-local receipt
/// records.  It is an in-memory observation only: it does not complete normal progress, release
/// claims, perform cleanup, establish a terminal disposition, or issue any live participant
/// authority.  A fresh private authority must validate a candidate before this value is stored.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AfterEffectReconciliationObservation {
    recovery_inventory: CancellationEffectRecoveryInventory,
    source_closure_observation: CancellationSourceClosureObservationRecord,
    coordinator_closure_observation: CancellationCoordinatorClosureObservationRecord,
    execution_epoch: u64,
    recovery_generation: u64,
}

impl AfterEffectReconciliationObservation {
    fn new(
        recovery_inventory: CancellationEffectRecoveryInventory,
        source_closure_observation: CancellationSourceClosureObservationRecord,
        coordinator_closure_observation: CancellationCoordinatorClosureObservationRecord,
        execution_epoch: u64,
        recovery_generation: u64,
    ) -> Self {
        Self {
            recovery_inventory,
            source_closure_observation,
            coordinator_closure_observation,
            execution_epoch,
            recovery_generation,
        }
    }

    /// Borrow the exact accepted after-effect recovery inventory.
    pub fn recovery_inventory(&self) -> &CancellationEffectRecoveryInventory {
        &self.recovery_inventory
    }

    /// Borrow the exact accepted source-closure observation.
    pub fn source_closure_observation(&self) -> &CancellationSourceClosureObservationRecord {
        &self.source_closure_observation
    }

    /// Borrow the exact accepted coordinator-closure observation.
    pub fn coordinator_closure_observation(
        &self,
    ) -> &CancellationCoordinatorClosureObservationRecord {
        &self.coordinator_closure_observation
    }

    /// Return the execution epoch accepted with this reconciliation state.
    pub const fn execution_epoch(&self) -> u64 {
        self.execution_epoch
    }

    /// Return the recovery-observation generation accepted with this state.
    pub const fn recovery_generation(&self) -> u64 {
        self.recovery_generation
    }
}

/// Immutable accepted cancellation-local resource and namespace observations.
///
/// This value is retained only as nonterminal observation state.  It does not prove adoption,
/// absence, cleanup capability, claim settlement, or a terminal cancellation outcome.  The
/// resource and namespace records retain the bounded recovery/dispatch lineage; this state keeps
/// only the accepted after-effect generation as its linkage.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CancellationCleanupObservation {
    resource_cleanup_observation: CancellationResourceCleanupObservationRecord,
    namespace_cleanup_observation: CancellationNamespaceCleanupObservationRecord,
    after_effect_generation: u64,
    execution_epoch: u64,
    recovery_generation: u64,
}

impl CancellationCleanupObservation {
    fn new(
        resource_cleanup_observation: CancellationResourceCleanupObservationRecord,
        namespace_cleanup_observation: CancellationNamespaceCleanupObservationRecord,
        after_effect_generation: u64,
        execution_epoch: u64,
        recovery_generation: u64,
    ) -> Self {
        Self {
            resource_cleanup_observation,
            namespace_cleanup_observation,
            after_effect_generation,
            execution_epoch,
            recovery_generation,
        }
    }

    /// Borrow the accepted resource-cleanup observations.
    pub fn resource_cleanup_observation(&self) -> &CancellationResourceCleanupObservationRecord {
        &self.resource_cleanup_observation
    }

    /// Borrow the accepted namespace/disposition observations.
    pub fn namespace_cleanup_observation(&self) -> &CancellationNamespaceCleanupObservationRecord {
        &self.namespace_cleanup_observation
    }

    /// Return the current execution epoch accepted with these observations.
    pub const fn execution_epoch(&self) -> u64 {
        self.execution_epoch
    }

    /// Return the global recovery generation accepted with these observations.
    pub const fn recovery_generation(&self) -> u64 {
        self.recovery_generation
    }

    /// Return the after-effect generation linked to these observations.
    pub const fn after_effect_generation(&self) -> u64 {
        self.after_effect_generation
    }
}

/// Failure while validating the bounded local shape of prepublication cleanup observations.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum PrePublishCleanupValidationError {
    /// The ordered action observations exceed the cancellation bound.
    #[error("prepublication cleanup has {actual} actions, maximum is {maximum}")]
    TooManyActions {
        /// Number of action rows supplied.
        actual: usize,
        /// Maximum number of action rows permitted.
        maximum: usize,
    },
    /// The action rows were not strictly sorted by ordinal or contained a duplicate.
    #[error(
        "prepublication cleanup action list is not sorted or unique at index {index}: {action_ordinal}"
    )]
    ActionsNotSortedOrUnique {
        /// Position of the offending row.
        index: usize,
        /// Offending action ordinal.
        action_ordinal: u8,
    },
}

/// Failure while validating the bounded local shape of unstarted-action quiescence observations.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum PrePublishUnstartedQuiescenceValidationError {
    /// The ordered action observations exceed the cancellation bound.
    #[error("prepublication unstarted quiescence has {actual} actions, maximum is {maximum}")]
    TooManyActions {
        /// Number of action rows supplied.
        actual: usize,
        /// Maximum number of action rows permitted.
        maximum: usize,
    },
    /// The aggregate key rows exceed the frozen-plan bound.
    #[error("prepublication unstarted quiescence has {actual} keys, maximum is {maximum}")]
    TooManyKeys {
        /// Number of keys supplied across all action rows.
        actual: usize,
        /// Maximum number of keys permitted.
        maximum: usize,
    },
    /// The action rows were not strictly sorted by ordinal or contained a duplicate.
    #[error(
        "prepublication unstarted quiescence action list is not sorted or unique at index {index}: {action_ordinal}"
    )]
    ActionsNotSortedOrUnique {
        /// Position of the offending row.
        index: usize,
        /// Offending action ordinal.
        action_ordinal: u8,
    },
    /// A row's eligible pending keys were not strictly sorted or contained a duplicate.
    #[error(
        "prepublication unstarted quiescence keys for action {action_ordinal} are not sorted or unique at index {index}"
    )]
    KeysNotSortedOrUnique {
        /// Action whose key list was invalid.
        action_ordinal: u8,
        /// Position of the offending key.
        index: usize,
    },
}

impl PrePublishCancellationObservation {
    /// Construct one stored prepublication cancellation observation.
    pub fn new(
        cleanup: PrePublishCleanupObservation,
        settlement: AdmissionClaimSettlementInventory,
    ) -> Self {
        Self::with_unstarted_quiescence(
            cleanup,
            PrePublishUnstartedQuiescenceObservation::new(Vec::new())
                .expect("empty quiescence observation is valid"),
            settlement,
        )
    }

    /// Construct one stored observation with an owner-supplied unstarted-action barrier result.
    pub fn with_unstarted_quiescence(
        cleanup: PrePublishCleanupObservation,
        unstarted_quiescence: PrePublishUnstartedQuiescenceObservation,
        settlement: AdmissionClaimSettlementInventory,
    ) -> Self {
        Self {
            cleanup,
            unstarted_quiescence,
            settlement,
            dispatch_outcome: None,
        }
    }

    /// Construct one stored observation with its accepted in-memory dispatch record.
    ///
    /// The record remains observational data until a private owner authority validates it
    /// against current progress; this constructor does not grant cleanup or terminal authority.
    pub fn with_dispatch_outcome(
        cleanup: PrePublishCleanupObservation,
        unstarted_quiescence: PrePublishUnstartedQuiescenceObservation,
        settlement: AdmissionClaimSettlementInventory,
        dispatch_outcome: CancellationDispatchOutcomeRecord,
    ) -> Self {
        Self {
            cleanup,
            unstarted_quiescence,
            settlement,
            dispatch_outcome: Some(dispatch_outcome),
        }
    }

    /// Borrow the bounded cleanup observations.
    pub fn cleanup(&self) -> &PrePublishCleanupObservation {
        &self.cleanup
    }

    /// Borrow the bounded unstarted-action quiescence observations.
    pub fn unstarted_quiescence(&self) -> &PrePublishUnstartedQuiescenceObservation {
        &self.unstarted_quiescence
    }

    /// Borrow the bounded claim settlement observations.
    pub fn settlement(&self) -> &AdmissionClaimSettlementInventory {
        &self.settlement
    }

    /// Borrow the accepted in-memory cancellation dispatch outcome, when present.
    pub fn dispatch_outcome(&self) -> Option<&CancellationDispatchOutcomeRecord> {
        self.dispatch_outcome.as_ref()
    }
}

impl WorkProgressStep {
    /// Return the exact immutable work-unit key.
    pub const fn key(&self) -> WorkUnitKey {
        self.key
    }

    /// Return the participant durability requirement.
    pub const fn requirement(&self) -> ParticipantRequirement {
        self.requirement
    }

    /// Return the current guarded status.
    pub const fn status(&self) -> ProgressStepStatus {
        self.status
    }

    /// Borrow the non-authoritative evidence retained for a completed step.
    pub fn evidence(&self) -> Option<&StoredStepEvidence> {
        self.evidence.as_ref()
    }

    /// Borrow the historical context accepted with a completed step.
    pub fn completion_context(&self) -> Option<&CompletionContext> {
        self.completion_context.as_ref()
    }
}

/// Result of applying a verified completion inside the crate.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProgressApplyOutcome {
    /// The completion advanced a pending step.
    Applied,
    /// The exact same completion was replayed idempotently.
    Replay,
}

/// Crate-private completion authority issued only by a future owner integration.
///
/// No production constructor exists in this module.  The test module creates deliberately scoped
/// values to exercise guards.  Keeping the full plan and participant scope here prevents a late
/// result from being accepted merely because its key or an opaque operation string matches.
#[allow(dead_code)]
pub(crate) struct VerifiedStepCompletion {
    operation_id: OperationId,
    plan: WorkPlan,
    key: WorkUnitKey,
    scope: ParticipantScope,
    execution_epoch: u64,
    evidence: StoredStepEvidence,
}

/// Crate-private owner-issued authority for settling all retained admission claims.
///
/// The authority is deliberately non-serialized and has no production constructor here.  A
/// future owner must bind the operation, frozen plan, current execution epoch, exact admitted
/// claim observation, and the candidate settlement observation before handing it to progress.
#[allow(dead_code)]
pub(crate) struct VerifiedClaimSettlement {
    operation_id: OperationId,
    plan: WorkPlan,
    execution_epoch: u64,
    admission_claims: AdmissionClaimInventory,
    settlement: AdmissionClaimSettlementInventory,
}

/// Private owner-issued authority for resuming work after a durable retry observation.
#[allow(dead_code)]
struct VerifiedRetryResume {
    operation_id: OperationId,
    plan: WorkPlan,
    retry_observation: RetryObservation,
    now_unix_ms: i64,
    expected_execution_epoch: u64,
}

/// Private owner-issued authority for reacquiring a source lease after a handoff.
///
/// The old scope is carried by value so a stale owner cannot cause the progress model to
/// rewrite a scope that another owner already reacquired.  This authority is intentionally not
/// constructible by production code in this module; the future source owner must issue it after
/// checking the live lease and source database.
#[allow(dead_code)]
struct VerifiedSourceReacquisition {
    operation_id: OperationId,
    plan: WorkPlan,
    execution_epoch: u64,
    target_participant_id: ParticipantId,
    old_scope: ParticipantScope,
    new_fence: StoredSourceFence,
}

/// Private owner-issued authority for resolving one exact active audit overlay.
#[allow(dead_code)]
struct VerifiedAuditResolution {
    operation_id: OperationId,
    plan: WorkPlan,
    execution_epoch: u64,
    generation: u64,
    observation: ProgressAuditObservation,
}

/// Private owner-issued authority for a bounded prepublication cleanup observation.
///
/// This authority is deliberately non-serialized and has no production constructor.  A future
/// owner must bind the operation, frozen plan, current execution epoch, frozen cancellation
/// inventory, current recovery-observation generation, and any unstarted-action quiescence
/// observation before handing it to progress.  `QuiescedWithoutEntry` remains a typed owner
/// observation; this module does not issue the barrier or inspect the filesystem.
#[allow(dead_code)]
#[derive(Clone)]
struct VerifiedPrePublishCancellation {
    operation_id: OperationId,
    plan: WorkPlan,
    execution_epoch: u64,
    cancellation_inventory: CancellationInventory,
    expected_recovery_generation: u64,
    cleanup: PrePublishCleanupObservation,
    unstarted_quiescence: PrePublishUnstartedQuiescenceObservation,
    settlement: AdmissionClaimSettlementInventory,
    dispatch_outcome: CancellationDispatchOutcomeRecord,
}

/// Private owner-issued authority for retaining one current after-effect reconciliation state.
///
/// The authority is deliberately non-serialized and has no production constructor.  A future
/// owner must bind the operation, exact plan, current execution epoch, frozen cancellation
/// inventory, and explicit refined cancellation-local records before handing it to progress.
#[allow(dead_code)]
#[derive(Clone)]
struct VerifiedAfterEffectReconciliation {
    operation_id: OperationId,
    plan: WorkPlan,
    execution_epoch: u64,
    cancellation_inventory: CancellationInventory,
    expected_recovery_generation: u64,
    recovery_inventory: CancellationEffectRecoveryInventory,
    source_closure_observation: CancellationSourceClosureObservationRecord,
    coordinator_closure_observation: CancellationCoordinatorClosureObservationRecord,
}

/// Private owner-issued authority for retaining resource and namespace cleanup observations.
///
/// The authority is deliberately non-serialized and has no production constructor.  The owner
/// must bind the current cancellation route, exact accepted after-effect state, current epoch,
/// and both canonical observation records before handing it to progress.
#[allow(dead_code)]
#[derive(Clone)]
struct VerifiedCancellationCleanupObservations {
    operation_id: OperationId,
    plan: WorkPlan,
    execution_epoch: u64,
    cancellation_inventory: CancellationInventory,
    expected_recovery_generation: u64,
    after_effect_reconciliation: AfterEffectReconciliationObservation,
    resource_cleanup_observation: CancellationResourceCleanupObservationRecord,
    namespace_cleanup_observation: CancellationNamespaceCleanupObservationRecord,
}

/// Private owner-issued authority for accepting one exact current readiness deferral batch.
///
/// The full plan is retained only in this non-serialized authority and in WorkProgress itself;
/// accepted history stores the compact observation value instead of duplicating the plan.
#[allow(dead_code)]
pub(crate) struct VerifiedReadinessDeferral {
    operation_id: OperationId,
    plan: WorkPlan,
    execution_epoch: u64,
    expected_previous_generation: u64,
    observation: ReadinessSchedulingObservationRecord,
}

/// Origin state required by one prepublication cleanup action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PrePublishOriginPolicy {
    OriginalStatePreserved,
    NoOriginalObject,
}

/// Immutable action policy derived from the command and frozen plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PrePublishActionPolicy {
    action_ordinal: u8,
    origin: PrePublishOriginPolicy,
}

/// Legacy placeholder for the not-yet-implemented post-publication cancellation outcome.
#[allow(dead_code)]
struct PostPublishCancellationAuthority {
    operation_id: OperationId,
    plan: WorkPlan,
    _recovery: VerifiedPostPublishRecovery,
}

/// Placeholder marker retained until the cancellation outcome gate is implemented.
#[allow(dead_code)]
struct VerifiedPostPublishRecovery;

/// Pure guarded progress over one exact frozen work plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkProgress {
    operation_id: OperationId,
    plan: WorkPlan,
    scopes: Vec<ParticipantScope>,
    steps: Vec<WorkProgressStep>,
    phase: OperationPhase,
    disposition: FullProgressDisposition,
    retry_observation: Option<RetryObservation>,
    last_retry_observation: Option<RetryObservation>,
    execution_epoch: u64,
    active_audit: Option<ActiveAudit>,
    audit_generation: u64,
    cancellation_request: Option<CancellationRequestObservation>,
    admission_claims: Option<AdmissionClaimInventory>,
    claim_settlement: Option<AdmissionClaimSettlementInventory>,
    cancellation_inventory: Option<CancellationInventory>,
    origin_policies: Vec<PrePublishActionPolicy>,
    recovery_generation: u64,
    pre_publish_cancellation: Option<PrePublishCancellationObservation>,
    after_effect_reconciliation: Option<AfterEffectReconciliationObservation>,
    cancellation_cleanup_observations: Option<CancellationCleanupObservation>,
    readiness_deferral_batches: Vec<ReadinessDeferralBatchObservation>,
}

#[allow(dead_code)]
impl WorkProgress {
    /// Derive exact initial step states from a validated frozen plan and initial inventory.
    pub fn from_plan(
        operation_id: OperationId,
        plan: &WorkPlan,
        args: &UniversalCommandArgs,
        participants: &ParticipantSet,
    ) -> Result<Self, WorkProgressError> {
        if operation_id.as_uuid().is_nil() {
            return Err(WorkProgressError::InvalidOperationId);
        }
        plan.validate_against(args, participants)?;
        let scopes = participants
            .descriptors()
            .iter()
            .map(ParticipantScope::from_descriptor)
            .collect::<Vec<_>>();
        let mut steps = Vec::with_capacity(plan.units().len());
        for unit in plan.units() {
            if !scopes
                .iter()
                .any(|scope| scope.participant_id == unit.key().participant_id())
            {
                return Err(WorkProgressError::ParticipantScopeMissing {
                    participant_id: unit.key().participant_id(),
                });
            }
            steps.push(WorkProgressStep {
                key: unit.key(),
                requirement: unit.requirement(),
                status: match unit.applicability() {
                    WorkUnitApplicability::Required => ProgressStepStatus::Pending,
                    WorkUnitApplicability::NotApplicable => ProgressStepStatus::NotApplicable,
                },
                evidence: None,
                completion_context: None,
            });
        }
        Ok(Self {
            operation_id,
            plan: plan.clone(),
            scopes,
            steps,
            phase: OperationPhase::IntentDurable,
            disposition: FullProgressDisposition::None,
            retry_observation: None,
            last_retry_observation: None,
            execution_epoch: 0,
            active_audit: None,
            audit_generation: 0,
            cancellation_request: None,
            admission_claims: None,
            claim_settlement: None,
            cancellation_inventory: None,
            origin_policies: derive_origin_policies(args, plan),
            recovery_generation: 0,
            pre_publish_cancellation: None,
            after_effect_reconciliation: None,
            cancellation_cleanup_observations: None,
            readiness_deferral_batches: Vec::new(),
        })
    }

    /// Derive progress after checking and retaining an immutable capacity-claim observation.
    ///
    /// Claim validation occurs before this method constructs or clones any progress state.  The
    /// retained inventory remains an observation only; it is not a reservation or admission
    /// authority.
    pub(crate) fn from_plan_with_admission_observations(
        operation_id: OperationId,
        plan: &WorkPlan,
        args: &UniversalCommandArgs,
        participants: &ParticipantSet,
        capacity_plan: &super::full_capacity::FullCapacityPlan,
        admission_claims: AdmissionClaimInventory,
    ) -> Result<Self, WorkProgressError> {
        admission_claims.validate_against(capacity_plan)?;
        let mut progress = Self::from_plan(operation_id, plan, args, participants)?;
        progress.admission_claims = Some(admission_claims);
        Ok(progress)
    }

    /// Return the operation identity bound to every completion.
    pub const fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    /// Borrow the exact frozen plan used to derive this progress.
    pub fn plan(&self) -> &WorkPlan {
        &self.plan
    }

    /// Return the monotonic visibility phase derived from completed obligations.
    pub const fn phase(&self) -> OperationPhase {
        self.phase
    }

    /// Return the guarded durable disposition.
    pub const fn disposition(&self) -> FullProgressDisposition {
        self.disposition
    }

    /// Borrow every exact keyed step in frozen plan order.
    pub fn steps(&self) -> &[WorkProgressStep] {
        &self.steps
    }

    /// Borrow compact accepted optional-artifact deferral history in generation order.
    pub fn readiness_deferral_batches(&self) -> &[ReadinessDeferralBatchObservation] {
        &self.readiness_deferral_batches
    }

    /// Validate retained completion observations against their accepted historical context.
    ///
    /// This is an observational check only.  It uses each completed step's accepted participant
    /// scope and evidence, allowing a stable source fence to be rebound while rejecting malformed
    /// or future historical contexts.
    pub fn validate_completion_history(&self) -> Result<(), WorkProgressError> {
        for step in &self.steps {
            match (
                step.status,
                step.evidence.as_ref(),
                step.completion_context.as_ref(),
            ) {
                (ProgressStepStatus::Completed, Some(evidence), Some(context)) => {
                    self.validate_normal_historical_completion(step.key, context, evidence)?;
                }
                (ProgressStepStatus::Completed, None, Some(_)) => {
                    return Err(WorkProgressError::CompletionEvidenceMissing { key: step.key });
                }
                (ProgressStepStatus::Completed, Some(_), None) => {
                    return Err(WorkProgressError::CompletionContextMissing { key: step.key });
                }
                (ProgressStepStatus::Completed, None, None) => {
                    return Err(WorkProgressError::CompletionContextMissing { key: step.key });
                }
                (_, Some(_), _) => {
                    return Err(WorkProgressError::CompletionEvidenceUnexpected { key: step.key });
                }
                (_, _, Some(_)) => {
                    return Err(WorkProgressError::CompletionContextUnexpected { key: step.key });
                }
                (_, None, None) => {}
            }
        }
        self.validate_readiness_deferral_history()?;
        Ok(())
    }

    /// Validate compact accepted optional-artifact deferral history.
    pub(crate) fn validate_readiness_deferral_history(&self) -> Result<(), WorkProgressError> {
        let mut expected_generation = 1_u64;
        let mut previous_epoch = 0_u64;
        let mut previous_artifacts = BTreeSet::new();
        let mut deferred_artifacts = BTreeSet::new();
        let mut previous_rows = BTreeMap::new();

        for (batch_index, batch) in self.readiness_deferral_batches.iter().enumerate() {
            batch
                .validate_shape()
                .map_err(WorkProgressError::ReadinessSchedulingObservation)?;
            if batch.observation_generation() != expected_generation {
                return Err(WorkProgressError::ReadinessDeferralGenerationMismatch {
                    expected: expected_generation,
                    actual: batch.observation_generation(),
                });
            }
            if batch.observation_epoch() > self.execution_epoch
                || batch.observation_epoch() < previous_epoch
            {
                return Err(WorkProgressError::ReadinessDeferralEpochInvalid {
                    epoch: batch.observation_epoch(),
                    previous: previous_epoch,
                    current: self.execution_epoch,
                });
            }
            let manifest = self.readiness_deferral_manifest(batch.artifact_keys())?;
            if manifest.readiness_keys().len() != batch.rows().len()
                || manifest
                    .readiness_keys()
                    .iter()
                    .zip(batch.rows())
                    .any(|(expected, row)| *expected != row.readiness_key())
            {
                return Err(WorkProgressError::ReadinessDeferralRowCoverageMismatch);
            }
            for artifact in batch.artifact_keys() {
                if !previous_artifacts.insert(*artifact) {
                    return Err(WorkProgressError::ReadinessDeferralArtifactOverlap {
                        key: *artifact,
                    });
                }
                deferred_artifacts.insert(*artifact);
                let step = self
                    .steps
                    .iter()
                    .find(|step| step.key == *artifact)
                    .ok_or(WorkProgressError::UnknownWorkUnit { key: *artifact })?;
                if step.status != ProgressStepStatus::Deferred {
                    return Err(WorkProgressError::ReadinessDeferralStatusMismatch {
                        key: *artifact,
                    });
                }
            }
            for row in batch.rows() {
                if row.context().execution_epoch() != batch.observation_epoch() {
                    return Err(WorkProgressError::ReadinessDeferralRowEpochMismatch {
                        key: row.readiness_key(),
                        actual: row.context().execution_epoch(),
                        expected: batch.observation_epoch(),
                    });
                }
                let readiness_step = self
                    .steps
                    .iter()
                    .find(|step| step.key == row.readiness_key())
                    .ok_or(WorkProgressError::UnknownWorkUnit {
                        key: row.readiness_key(),
                    })?;
                let (normal_context, normal_row) = match (
                    readiness_step.status,
                    readiness_step.completion_context.as_ref(),
                    readiness_step.evidence.as_ref(),
                ) {
                    (
                        ProgressStepStatus::Completed,
                        Some(context),
                        Some(StoredStepEvidence::ReadinessScheduled { observation }),
                    ) => (context, observation),
                    _ => {
                        return Err(WorkProgressError::ReadinessDeferralReadinessIncomplete {
                            key: row.readiness_key(),
                        });
                    }
                };
                if normal_context.execution_epoch() > batch.observation_epoch() {
                    return Err(WorkProgressError::ReadinessDeferralReadinessEpochInvalid {
                        key: row.readiness_key(),
                    });
                }
                validate_readiness_deferral_source_revision_order(
                    row,
                    normal_row,
                    &self.readiness_deferral_batches[..batch_index],
                )?;
                self.validate_historical_completion(
                    row.readiness_key(),
                    row.context(),
                    &StoredStepEvidence::ReadinessScheduled {
                        observation: row.clone(),
                    },
                )?;
                validate_readiness_publication_order(
                    Some(normal_row),
                    previous_rows.get(&row.readiness_key()),
                    row,
                )?;
                previous_rows.insert(row.readiness_key(), row.clone());
            }
            expected_generation = expected_generation
                .checked_add(1)
                .ok_or(WorkProgressError::ReadinessDeferralGenerationOverflow)?;
            previous_epoch = batch.observation_epoch();
        }

        let actual_deferred = self
            .steps
            .iter()
            .filter(|step| step.status == ProgressStepStatus::Deferred)
            .map(|step| step.key)
            .collect::<BTreeSet<_>>();
        if actual_deferred != deferred_artifacts {
            return Err(WorkProgressError::ReadinessDeferralStatusCoverageMismatch);
        }
        Ok(())
    }

    /// Validate one retained or late completion against its accepted historical context.
    ///
    /// This shared observational path keeps cancellation late-completion records subject to the
    /// same scope, evidence, fence, and historical revision rules as normal completion history.
    fn validate_historical_completion(
        &self,
        key: WorkUnitKey,
        context: &CompletionContext,
        evidence: &StoredStepEvidence,
    ) -> Result<(), WorkProgressError> {
        let current_scope = self.validate_historical_context_for_unit(key, context)?;
        evidence.validate()?;
        if let (Some(current_fence), Some(historical_fence)) =
            (current_scope.source_fence(), context.scope.source_fence())
        {
            if historical_fence.lease_epoch() < current_fence.lease_epoch() {
                if let Some(observation) = source_observation_for_evidence(evidence) {
                    if observation.revision() > current_fence.source_revision() {
                        return Err(
                            WorkProgressError::CompletionHistoryRevisionBeyondCurrentFence {
                                key,
                                observed: observation.revision(),
                                current_revision: current_fence.source_revision(),
                            },
                        );
                    }
                }
            }
        }
        validate_evidence_for_role(key.role(), &context.scope, evidence, None)?;
        validate_readiness_scheduled_binding(key, context, evidence)
    }

    fn validate_normal_historical_completion(
        &self,
        key: WorkUnitKey,
        context: &CompletionContext,
        evidence: &StoredStepEvidence,
    ) -> Result<(), WorkProgressError> {
        self.validate_historical_completion(key, context, evidence)?;
        require_normal_readiness_evidence(key, evidence)
    }

    /// Validate the historical epoch and participant scope retained for one unit.
    ///
    /// Resource-cleanup observations intentionally retain context without synthesizing step
    /// evidence.  This prefix is shared with normal completion-history validation so source
    /// reacquisition and stable-identity rules remain identical across both observation paths.
    fn validate_historical_context_for_unit(
        &self,
        key: WorkUnitKey,
        context: &CompletionContext,
    ) -> Result<&ParticipantScope, WorkProgressError> {
        let current_scope =
            self.scope_for(key)
                .ok_or(WorkProgressError::ParticipantScopeMissing {
                    participant_id: key.participant_id(),
                })?;
        if context.execution_epoch > self.execution_epoch {
            return Err(WorkProgressError::CompletionContextEpochFuture {
                key,
                actual: context.execution_epoch,
                current: self.execution_epoch,
            });
        }
        if !historical_scope_matches_current(
            self.execution_epoch,
            context.execution_epoch,
            current_scope,
            &context.scope,
        ) {
            return Err(WorkProgressError::CompletionContextScopeMismatch { key });
        }
        if let Some(fence) = context.scope.source_fence() {
            fence.validate()?;
        }
        Ok(current_scope)
    }

    /// Find one exact keyed step.
    pub fn step_for(&self, key: WorkUnitKey) -> Option<&WorkProgressStep> {
        self.steps.iter().find(|step| step.key == key)
    }

    /// Borrow the retained retry observation, when retry is pending.
    pub fn retry_observation(&self) -> Option<&RetryObservation> {
        self.retry_observation.as_ref()
    }

    /// Borrow the most recently requested retry observation, including a completed retry.
    pub fn last_retry_observation(&self) -> Option<&RetryObservation> {
        self.last_retry_observation.as_ref()
    }

    /// Return the private execution epoch bound to new completions.
    pub const fn execution_epoch(&self) -> u64 {
        self.execution_epoch
    }

    /// Borrow the retained audit observation, when progress is audit-blocked.
    pub fn audit_observation(&self) -> Option<&ProgressAuditObservation> {
        self.active_audit.as_ref().map(|audit| &audit.observation)
    }

    /// Return the retained audit generation, including generations that were resolved.
    pub const fn audit_generation(&self) -> u64 {
        self.audit_generation
    }

    /// Borrow the retained cancellation request, when cancellation was requested.
    pub const fn cancellation_request(&self) -> Option<CancellationRequestObservation> {
        self.cancellation_request
    }

    /// Borrow the optional immutable capacity-claim observation retained with progress.
    pub fn admission_claims(&self) -> Option<&AdmissionClaimInventory> {
        self.admission_claims.as_ref()
    }

    /// Borrow the optional immutable settlement observation retained by the success guard.
    pub fn claim_settlement(&self) -> Option<&AdmissionClaimSettlementInventory> {
        self.claim_settlement.as_ref()
    }

    /// Borrow the immutable cancellation inventory frozen by the first request, when present.
    pub fn cancellation_inventory(&self) -> Option<&CancellationInventory> {
        self.cancellation_inventory.as_ref()
    }

    /// Derive the bounded effect inventory from the exact frozen cancellation source.
    ///
    /// The derivation validates the retained inventory against this operation's current plan,
    /// steps, claims, and execution epoch before it produces any effect rows.  It is an
    /// observation only: it does not grant recovery, filesystem, source, or terminal authority.
    pub fn cancellation_effect_inventory(
        &self,
    ) -> Result<CancellationEffectInventory, CancellationEffectInventoryError> {
        let inventory = self
            .cancellation_inventory
            .as_ref()
            .ok_or(CancellationEffectInventoryError::CancellationInventoryMissing)?;
        let admission_claims = self
            .admission_claims
            .as_ref()
            .ok_or(CancellationEffectInventoryError::AdmissionClaimsMissing)?;
        derive_effect_inventory(
            self.operation_id,
            &self.plan,
            inventory,
            &self.steps,
            admission_claims,
            self.execution_epoch,
        )
    }

    /// Derive bounded after-effect observations from the exact frozen cancellation inventory and
    /// one validated cancellation-wide dispatch outcome record.
    ///
    /// Only `ObservedAfterCancellation` rows overlay temporary completed statuses for local
    /// recipe derivation.  The returned value retains the full frozen inventory and outcome
    /// record; it grants no recovery, filesystem, source, cleanup, settlement, or terminal
    /// authority.
    pub fn cancellation_effect_recovery_inventory(
        &self,
        record: &CancellationDispatchOutcomeRecord,
    ) -> Result<CancellationEffectRecoveryInventory, WorkProgressError> {
        self.validate_completion_history()?;
        self.validate_cancellation_dispatch_record(record)?;
        let inventory = self
            .cancellation_inventory
            .as_ref()
            .ok_or(WorkProgressError::CancellationInventoryMissing)?;
        let admission_claims = self
            .admission_claims
            .as_ref()
            .ok_or(WorkProgressError::CancellationAdmissionClaimsMissing)?;
        derive_effect_recovery_inventory(
            self.operation_id,
            &self.plan,
            inventory,
            record,
            &self.steps,
            admission_claims,
            self.execution_epoch,
        )
        .map_err(WorkProgressError::CancellationEffectInventory)
    }

    /// Validate one candidate against an explicitly supplied expected dispatch outcome and the
    /// exact retained frozen inventory, then recompute its canonical refined bundles.  This is an
    /// observational check only; callers choose whether the expected record is a new candidate
    /// observation or an already accepted in-memory record.
    pub fn validate_cancellation_effect_recovery_inventory(
        &self,
        candidate: &CancellationEffectRecoveryInventory,
        expected_dispatch_outcome: &CancellationDispatchOutcomeRecord,
    ) -> Result<(), WorkProgressError> {
        self.validate_completion_history()?;
        let inventory = self
            .cancellation_inventory
            .as_ref()
            .ok_or(WorkProgressError::CancellationInventoryMissing)?;
        let admission_claims = self
            .admission_claims
            .as_ref()
            .ok_or(WorkProgressError::CancellationAdmissionClaimsMissing)?;
        self.validate_cancellation_dispatch_record(candidate.dispatch_outcome())?;
        self.validate_cancellation_dispatch_record(expected_dispatch_outcome)?;
        candidate
            .validate_against(
                self.operation_id,
                &self.plan,
                inventory,
                expected_dispatch_outcome,
                &self.steps,
                admission_claims,
                self.execution_epoch,
            )
            .map_err(WorkProgressError::CancellationEffectInventory)
    }

    /// Derive the exact canonical cleanup-obligation manifest from explicit recovery lineage.
    ///
    /// Every non-global frozen plan segment is retained when it matches one of the strict V1
    /// cancellation recipes, including preparation-only and pending segments with no effect
    /// bundle.  The result is observational only: it does not prove no entry, absence, adoption,
    /// release, or terminal completion.
    pub fn cancellation_cleanup_manifest(
        &self,
        recovery_inventory: &CancellationEffectRecoveryInventory,
        expected_dispatch_outcome: &CancellationDispatchOutcomeRecord,
    ) -> Result<CancellationCleanupManifest, WorkProgressError> {
        self.validate_cancellation_effect_recovery_inventory(
            recovery_inventory,
            expected_dispatch_outcome,
        )?;
        let admission_claims = self
            .admission_claims
            .as_ref()
            .ok_or(WorkProgressError::CancellationAdmissionClaimsMissing)?;
        derive_cancellation_cleanup_manifest(
            self.operation_id,
            &self.plan,
            recovery_inventory,
            expected_dispatch_outcome,
            &self.steps,
            admission_claims,
            self.execution_epoch,
        )
        .map_err(WorkProgressError::CancellationCleanupManifest)
    }

    /// Validate one cleanup-obligation candidate against explicit recovery and dispatch lineage.
    ///
    /// The expected values are explicit so an observational candidate cannot silently substitute
    /// a retained accepted state.  This check never mutates progress or grants cleanup authority.
    pub fn validate_cancellation_cleanup_manifest(
        &self,
        candidate: &CancellationCleanupManifest,
        expected_recovery_inventory: &CancellationEffectRecoveryInventory,
        expected_dispatch_outcome: &CancellationDispatchOutcomeRecord,
    ) -> Result<(), WorkProgressError> {
        self.validate_cancellation_effect_recovery_inventory(
            expected_recovery_inventory,
            expected_dispatch_outcome,
        )?;
        let admission_claims = self
            .admission_claims
            .as_ref()
            .ok_or(WorkProgressError::CancellationAdmissionClaimsMissing)?;
        candidate
            .validate_against(
                self.operation_id,
                &self.plan,
                expected_recovery_inventory,
                expected_dispatch_outcome,
                &self.steps,
                admission_claims,
                self.execution_epoch,
            )
            .map_err(WorkProgressError::CancellationCleanupManifest)
    }

    /// Derive the exact applicable staging and retained-recovery resource manifest from one
    /// explicitly supplied cleanup lineage.
    ///
    /// The returned value is observational only.  It does not complete normal progress, prove
    /// adoption or absence, discharge operation-wide claims, or authorize cleanup or release.
    pub fn cancellation_resource_cleanup_manifest(
        &self,
        expected_cleanup_manifest: &CancellationCleanupManifest,
        expected_recovery_inventory: &CancellationEffectRecoveryInventory,
        expected_dispatch_outcome: &CancellationDispatchOutcomeRecord,
    ) -> Result<CancellationResourceCleanupManifest, WorkProgressError> {
        self.validate_cancellation_cleanup_manifest(
            expected_cleanup_manifest,
            expected_recovery_inventory,
            expected_dispatch_outcome,
        )?;
        CancellationResourceCleanupManifest::derive(expected_cleanup_manifest).map_err(|error| {
            WorkProgressError::CancellationResourceCleanupManifest(
                CancellationResourceCleanupManifestError::from(error),
            )
        })
    }

    /// Validate one candidate resource-cleanup manifest against explicit cleanup lineage.
    pub fn validate_cancellation_resource_cleanup_manifest(
        &self,
        candidate: &CancellationResourceCleanupManifest,
        expected_cleanup_manifest: &CancellationCleanupManifest,
        expected_recovery_inventory: &CancellationEffectRecoveryInventory,
        expected_dispatch_outcome: &CancellationDispatchOutcomeRecord,
    ) -> Result<(), WorkProgressError> {
        let expected = self.cancellation_resource_cleanup_manifest(
            expected_cleanup_manifest,
            expected_recovery_inventory,
            expected_dispatch_outcome,
        )?;
        candidate
            .validate_against(&expected)
            .map_err(WorkProgressError::CancellationResourceCleanupManifest)
    }

    /// Validate one immutable cancellation-local resource-cleanup observation record.
    ///
    /// The expected cleanup manifest, recovery inventory, and dispatch outcome are explicit so
    /// a candidate cannot silently substitute retained state.  Preserved and absent rows retain
    /// historical participant context only; they do not synthesize completion evidence or affect
    /// source-revision floors.
    pub fn validate_cancellation_resource_cleanup_observation_record(
        &self,
        candidate: &CancellationResourceCleanupObservationRecord,
        expected_cleanup_manifest: &CancellationCleanupManifest,
        expected_recovery_inventory: &CancellationEffectRecoveryInventory,
        expected_dispatch_outcome: &CancellationDispatchOutcomeRecord,
    ) -> Result<(), WorkProgressError> {
        let manifest = self.cancellation_resource_cleanup_manifest(
            expected_cleanup_manifest,
            expected_recovery_inventory,
            expected_dispatch_outcome,
        )?;
        candidate
            .validate_against(&manifest, self.execution_epoch)
            .map_err(WorkProgressError::CancellationResourceCleanupObservation)?;
        for row in candidate.rows() {
            let context = match row.outcome() {
                CancellationResourceCleanupOutcome::Pending => continue,
                CancellationResourceCleanupOutcome::Preserved { context, .. }
                | CancellationResourceCleanupOutcome::Absent { context, .. } => context,
            };
            self.validate_historical_context_for_unit(row.key(), context)?;
        }
        Ok(())
    }

    /// Derive the exact applicable namespace/disposition manifest from one explicit cleanup
    /// lineage.
    ///
    /// The returned value is observational only.  It does not complete normal progress, prove
    /// absence, adoption, rollback, qualification, or authorize cleanup or release.
    pub fn cancellation_namespace_cleanup_manifest(
        &self,
        expected_cleanup_manifest: &CancellationCleanupManifest,
        expected_recovery_inventory: &CancellationEffectRecoveryInventory,
        expected_dispatch_outcome: &CancellationDispatchOutcomeRecord,
    ) -> Result<CancellationNamespaceCleanupManifest, WorkProgressError> {
        self.validate_cancellation_cleanup_manifest(
            expected_cleanup_manifest,
            expected_recovery_inventory,
            expected_dispatch_outcome,
        )?;
        CancellationNamespaceCleanupManifest::derive(expected_cleanup_manifest).map_err(|error| {
            WorkProgressError::CancellationNamespaceCleanupManifest(
                CancellationNamespaceCleanupManifestError::from(error),
            )
        })
    }

    /// Validate one namespace/disposition manifest against explicit cleanup lineage.
    pub fn validate_cancellation_namespace_cleanup_manifest(
        &self,
        candidate: &CancellationNamespaceCleanupManifest,
        expected_cleanup_manifest: &CancellationCleanupManifest,
        expected_recovery_inventory: &CancellationEffectRecoveryInventory,
        expected_dispatch_outcome: &CancellationDispatchOutcomeRecord,
    ) -> Result<(), WorkProgressError> {
        let expected = self.cancellation_namespace_cleanup_manifest(
            expected_cleanup_manifest,
            expected_recovery_inventory,
            expected_dispatch_outcome,
        )?;
        candidate
            .validate_against(&expected)
            .map_err(WorkProgressError::CancellationNamespaceCleanupManifest)
    }

    /// Validate one immutable cancellation-local namespace/disposition observation record.
    ///
    /// The expected cleanup manifest, recovery inventory, and dispatch outcome are explicit so
    /// an observational candidate cannot silently substitute retained state.  Rows retain
    /// historical participant context only; they do not synthesize completion evidence, affect
    /// source-revision floors, or change normal/frozen progress.
    pub fn validate_cancellation_namespace_cleanup_observation_record(
        &self,
        candidate: &CancellationNamespaceCleanupObservationRecord,
        expected_cleanup_manifest: &CancellationCleanupManifest,
        expected_recovery_inventory: &CancellationEffectRecoveryInventory,
        expected_dispatch_outcome: &CancellationDispatchOutcomeRecord,
    ) -> Result<(), WorkProgressError> {
        let manifest = self.cancellation_namespace_cleanup_manifest(
            expected_cleanup_manifest,
            expected_recovery_inventory,
            expected_dispatch_outcome,
        )?;
        candidate
            .validate_against(&manifest, self.execution_epoch)
            .map_err(WorkProgressError::CancellationNamespaceCleanupObservation)?;
        for row in candidate.rows() {
            let context = match row.outcome() {
                CancellationNamespaceCleanupOutcome::Pending => continue,
                CancellationNamespaceCleanupOutcome::NoOperationClaim { context, .. }
                | CancellationNamespaceCleanupOutcome::Preserved { context, .. }
                | CancellationNamespaceCleanupOutcome::Absent { context, .. } => context,
            };
            self.validate_historical_context_for_unit(row.key(), context)?;
        }
        Ok(())
    }

    /// Derive the exact pending source-closure manifest from one validated after-effect inventory.
    ///
    /// The manifest retains the full recovery lineage and is an observation only.  It does not
    /// complete normal progress, issue source authority, or authorize cleanup or settlement.
    pub fn cancellation_source_closure_manifest(
        &self,
        recovery_inventory: &CancellationEffectRecoveryInventory,
        expected_dispatch_outcome: &CancellationDispatchOutcomeRecord,
    ) -> Result<SourceClosureManifest, WorkProgressError> {
        self.validate_cancellation_effect_recovery_inventory(
            recovery_inventory,
            expected_dispatch_outcome,
        )?;
        SourceClosureManifest::derive(recovery_inventory.clone()).map_err(|error| {
            WorkProgressError::CancellationSourceClosureManifest(SourceClosureManifestError::from(
                error,
            ))
        })
    }

    /// Validate one candidate source-closure manifest against an explicitly supplied recovery
    /// inventory and dispatch lineage.
    ///
    /// This is a read-only observational check.  The expected values are explicit so a caller
    /// can validate a candidate observation without implicitly consulting accepted state.
    pub fn validate_cancellation_source_closure_manifest(
        &self,
        candidate: &SourceClosureManifest,
        expected_recovery_inventory: &CancellationEffectRecoveryInventory,
        expected_dispatch_outcome: &CancellationDispatchOutcomeRecord,
    ) -> Result<(), WorkProgressError> {
        let expected = self.cancellation_source_closure_manifest(
            expected_recovery_inventory,
            expected_dispatch_outcome,
        )?;
        candidate
            .validate_against(&expected)
            .map_err(WorkProgressError::CancellationSourceClosureManifest)
    }

    /// Validate one immutable cancellation-local source-closure observation record.
    ///
    /// The record is checked against explicit recovery and dispatch lineage, current execution
    /// epoch, and the same historical participant/evidence rules used by retained completions.
    /// It does not mutate normal steps, frozen inventories, claims, or terminal state.
    pub fn validate_cancellation_source_closure_observation_record(
        &self,
        candidate: &CancellationSourceClosureObservationRecord,
        expected_recovery_inventory: &CancellationEffectRecoveryInventory,
        expected_dispatch_outcome: &CancellationDispatchOutcomeRecord,
    ) -> Result<(), WorkProgressError> {
        let manifest = self.cancellation_source_closure_manifest(
            expected_recovery_inventory,
            expected_dispatch_outcome,
        )?;
        candidate
            .validate_against(&manifest, self.execution_epoch)
            .map_err(WorkProgressError::CancellationSourceClosureObservation)?;
        for row in candidate.rows() {
            if let CancellationSourceClosureOutcome::Observed { evidence, context } = row.outcome()
            {
                self.validate_historical_completion(row.key(), context, evidence)?;
                if let Some(fence) = context.scope().source_fence() {
                    if let Some(minimum) = self.cancellation_source_revision_floor(
                        fence,
                        expected_dispatch_outcome,
                        None,
                    ) {
                        validate_evidence_for_role(
                            row.key().role(),
                            context.scope(),
                            evidence,
                            Some(minimum),
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Derive the exact cancellation-local coordinator-closure manifest from one validated
    /// source-closure observation and its complete recovery lineage.
    ///
    /// Required applicable global coordinator units are selected only when a cancellation-local
    /// effect or source commit exists.  Source-fenced coordinator units are narrowed to stable
    /// source identities linked by an observed effect closure or source commit.  Normal
    /// coordinator completion status is intentionally not used as cancellation evidence.
    pub fn cancellation_coordinator_closure_manifest(
        &self,
        source_closure_observation: &CancellationSourceClosureObservationRecord,
        expected_recovery_inventory: &CancellationEffectRecoveryInventory,
        expected_dispatch_outcome: &CancellationDispatchOutcomeRecord,
    ) -> Result<CoordinatorClosureManifest, WorkProgressError> {
        self.validate_cancellation_source_closure_observation_record(
            source_closure_observation,
            expected_recovery_inventory,
            expected_dispatch_outcome,
        )?;
        let keys = self.derive_cancellation_coordinator_closure_keys(
            expected_recovery_inventory,
            expected_dispatch_outcome,
        );
        CoordinatorClosureManifest::new(source_closure_observation.clone(), keys).map_err(|error| {
            WorkProgressError::CancellationCoordinatorClosureManifest(
                CoordinatorClosureManifestError::from(error),
            )
        })
    }

    /// Validate one candidate coordinator-closure manifest against explicit source, recovery,
    /// and dispatch lineage.
    pub fn validate_cancellation_coordinator_closure_manifest(
        &self,
        candidate: &CoordinatorClosureManifest,
        expected_source_closure_observation: &CancellationSourceClosureObservationRecord,
        expected_recovery_inventory: &CancellationEffectRecoveryInventory,
        expected_dispatch_outcome: &CancellationDispatchOutcomeRecord,
    ) -> Result<(), WorkProgressError> {
        let expected = self.cancellation_coordinator_closure_manifest(
            expected_source_closure_observation,
            expected_recovery_inventory,
            expected_dispatch_outcome,
        )?;
        candidate
            .validate_against(&expected)
            .map_err(WorkProgressError::CancellationCoordinatorClosureManifest)
    }

    /// Validate one immutable cancellation-local coordinator-closure observation record.
    ///
    /// The expected source-closure record, recovery inventory, and dispatch outcome are explicit
    /// so candidate validation cannot silently substitute retained state.  This method is
    /// observational only and does not complete normal coordinator steps or grant authority.
    pub fn validate_cancellation_coordinator_closure_observation_record(
        &self,
        candidate: &CancellationCoordinatorClosureObservationRecord,
        expected_source_closure_observation: &CancellationSourceClosureObservationRecord,
        expected_recovery_inventory: &CancellationEffectRecoveryInventory,
        expected_dispatch_outcome: &CancellationDispatchOutcomeRecord,
    ) -> Result<(), WorkProgressError> {
        let manifest = self.cancellation_coordinator_closure_manifest(
            expected_source_closure_observation,
            expected_recovery_inventory,
            expected_dispatch_outcome,
        )?;
        candidate
            .validate_against(&manifest, self.execution_epoch)
            .map_err(WorkProgressError::CancellationCoordinatorClosureObservation)?;
        for row in candidate.rows() {
            let CancellationCoordinatorClosureOutcome::Observed { evidence, context } =
                row.outcome()
            else {
                continue;
            };
            self.validate_historical_completion(row.key(), context, evidence)?;
            let Some(fence) = context.scope().source_fence() else {
                continue;
            };
            if let Some(minimum) = self.cancellation_source_revision_floor(
                fence,
                expected_dispatch_outcome,
                Some(expected_source_closure_observation),
            ) {
                validate_evidence_for_role(
                    row.key().role(),
                    context.scope(),
                    evidence,
                    Some(minimum),
                )?;
            }
        }
        Ok(())
    }

    /// Derive the exact cancellation-wide pending dispatch manifest from the frozen source.
    ///
    /// The manifest covers every applicable pending non-source-read unit in the frozen plan,
    /// including later history segments and global recipe units.  It remains an observation only:
    /// it does not issue a barrier or grant recovery, filesystem, source, or terminal authority.
    pub fn cancellation_dispatch_manifest(
        &self,
    ) -> Result<CancellationDispatchManifest, CancellationDispatchManifestError> {
        let inventory = self
            .cancellation_inventory
            .as_ref()
            .ok_or(CancellationDispatchManifestError::CancellationInventoryMissing)?;
        let admission_claims = self
            .admission_claims
            .as_ref()
            .ok_or(CancellationDispatchManifestError::AdmissionClaimsMissing)?;
        derive_dispatch_manifest(
            self.operation_id,
            &self.plan,
            inventory,
            &self.steps,
            admission_claims,
            self.execution_epoch,
        )
    }

    /// Validate one immutable cancellation-wide outcome record against current frozen progress.
    ///
    /// This checks only observational consistency.  It does not accept or store the record,
    /// establish a cancellation barrier, release a claim, perform cleanup, or grant authority.
    pub fn validate_cancellation_dispatch_record(
        &self,
        candidate: &CancellationDispatchOutcomeRecord,
    ) -> Result<(), WorkProgressError> {
        let manifest = self
            .cancellation_dispatch_manifest()
            .map_err(WorkProgressError::CancellationDispatchManifest)?;
        candidate
            .validate_against(&manifest, self.execution_epoch)
            .map_err(WorkProgressError::CancellationDispatchOutcome)?;
        for row in candidate.rows() {
            if let CancellationDispatchOutcome::ObservedAfterCancellation { evidence, context } =
                row.outcome()
            {
                self.validate_historical_completion(row.key(), context, evidence)?;
            }
        }
        Ok(())
    }

    /// Derive the exact readiness prerequisites for an explicit optional-artifact selection.
    ///
    /// This is a read-only planner observation.  It does not inspect step completion, defer an
    /// artifact, schedule a target, or establish a durable readiness owner.
    pub fn readiness_deferral_manifest(
        &self,
        selected_artifact_keys: &[WorkUnitKey],
    ) -> Result<ReadinessDeferralManifest, WorkProgressError> {
        derive_readiness_deferral_manifest(
            self.operation_id,
            &self.plan,
            &self.scopes,
            selected_artifact_keys,
        )
        .map_err(WorkProgressError::ReadinessDeferralManifest)
    }

    /// Validate one current-epoch readiness scheduling observation against an explicit selection.
    ///
    /// The record is an untrusted observation only.  Every row must bind the current participant
    /// scope and source fence, and its source revision must meet the existing accepted-progress
    /// floor.  No row is stored and this method cannot defer an Artifact or grant scheduling
    /// authority.
    pub fn validate_readiness_scheduling_observation_record(
        &self,
        candidate: &ReadinessSchedulingObservationRecord,
        selected_artifact_keys: &[WorkUnitKey],
    ) -> Result<(), WorkProgressError> {
        let manifest = self.readiness_deferral_manifest(selected_artifact_keys)?;
        candidate
            .validate_against(&manifest)
            .map_err(WorkProgressError::ReadinessSchedulingObservation)?;
        if candidate.observation_epoch() != self.execution_epoch {
            return Err(WorkProgressError::ReadinessSchedulingEpochMismatch {
                expected: self.execution_epoch,
                actual: candidate.observation_epoch(),
            });
        }
        for row in candidate.rows() {
            let current_scope = self.scope_for(row.readiness_key()).ok_or(
                WorkProgressError::ParticipantScopeMissing {
                    participant_id: row.readiness_key().participant_id(),
                },
            )?;
            if row.context().execution_epoch() != self.execution_epoch {
                return Err(WorkProgressError::ReadinessSchedulingContextEpochMismatch {
                    key: row.readiness_key(),
                    expected: self.execution_epoch,
                    actual: row.context().execution_epoch(),
                });
            }
            if row.context().scope() != current_scope {
                return Err(WorkProgressError::ReadinessSchedulingScopeMismatch {
                    key: row.readiness_key(),
                });
            }
            let fence = current_scope.source_fence().ok_or(
                WorkProgressError::ReadinessSchedulingSourceFenceMissing {
                    key: row.readiness_key(),
                },
            )?;
            let minimum_revision = self.minimum_source_revision_for_fence(fence);
            validate_source_observation(
                WorkUnitRole::Readiness,
                row.source_observation(),
                fence,
                minimum_revision,
            )?;
        }
        Ok(())
    }

    /// Accept one private, current-epoch after-effect reconciliation state without changing
    /// normal progress or deriving a terminal cancellation outcome.
    #[allow(dead_code)]
    fn accept_after_effect_reconciliation(
        &mut self,
        authority: VerifiedAfterEffectReconciliation,
    ) -> Result<(), WorkProgressError> {
        if authority.operation_id != self.operation_id || authority.plan != self.plan {
            return Err(WorkProgressError::AfterEffectReconciliationAuthorityMismatch);
        }
        if authority.execution_epoch != self.execution_epoch {
            return Err(WorkProgressError::AfterEffectReconciliationEpochMismatch {
                expected: self.execution_epoch,
                actual: authority.execution_epoch,
            });
        }
        if self.active_audit.is_some() || self.disposition == FullProgressDisposition::AuditRequired
        {
            return Err(WorkProgressError::AfterEffectReconciliationAuditActive);
        }
        if self.disposition.is_terminal() {
            return Err(WorkProgressError::TerminalTransitionForbidden);
        }
        let Some(cancellation_request) = self.cancellation_request else {
            return Err(WorkProgressError::CancellationNotRequested);
        };
        let Some(current_inventory) = self.cancellation_inventory.as_ref() else {
            return Err(WorkProgressError::CancellationInventoryMissing);
        };
        let expected_disposition = match current_inventory.route() {
            CancellationRoute::BeforePublish => {
                FullProgressDisposition::CancelRequestedBeforePublish
            }
            CancellationRoute::AfterPublish => FullProgressDisposition::CancelRequestedAfterPublish,
            CancellationRoute::AfterObservedEffect => {
                FullProgressDisposition::CancelRequestedAfterObservedEffect
            }
        };
        if self.disposition != expected_disposition {
            return Err(WorkProgressError::AfterEffectReconciliationStateInvalid);
        }
        if authority.cancellation_inventory != *current_inventory {
            return Err(WorkProgressError::AfterEffectReconciliationInventoryMismatch);
        }
        authority.cancellation_inventory.validate_against(
            self.operation_id,
            &self.plan,
            &self.steps,
            self.execution_epoch,
            self.admission_claims
                .as_ref()
                .ok_or(WorkProgressError::CancellationAdmissionClaimsMissing)?,
        )?;
        let dispatch_outcome = authority.recovery_inventory.dispatch_outcome();
        if dispatch_outcome.observation_epoch() != self.execution_epoch {
            return Err(
                WorkProgressError::AfterEffectReconciliationDispatchEpochMismatch {
                    expected: self.execution_epoch,
                    actual: dispatch_outcome.observation_epoch(),
                },
            );
        }
        if authority.source_closure_observation.observation_epoch() != self.execution_epoch {
            return Err(
                WorkProgressError::AfterEffectReconciliationSourceClosureEpochMismatch {
                    expected: self.execution_epoch,
                    actual: authority.source_closure_observation.observation_epoch(),
                },
            );
        }
        if authority
            .coordinator_closure_observation
            .observation_epoch()
            != self.execution_epoch
        {
            return Err(
                WorkProgressError::AfterEffectReconciliationCoordinatorClosureEpochMismatch {
                    expected: self.execution_epoch,
                    actual: authority
                        .coordinator_closure_observation
                        .observation_epoch(),
                },
            );
        }

        // Validate every explicit lineage and nested historical receipt before consulting or
        // changing the accepted after-effect state.  These methods intentionally take explicit
        // expected records, so a candidate cannot silently substitute retained state.
        self.validate_completion_history()?;
        self.validate_cancellation_effect_recovery_inventory(
            &authority.recovery_inventory,
            dispatch_outcome,
        )?;
        self.validate_cancellation_source_closure_observation_record(
            &authority.source_closure_observation,
            &authority.recovery_inventory,
            dispatch_outcome,
        )?;
        self.validate_cancellation_coordinator_closure_observation_record(
            &authority.coordinator_closure_observation,
            &authority.source_closure_observation,
            &authority.recovery_inventory,
            dispatch_outcome,
        )?;
        validate_after_effect_phase_gates(
            dispatch_outcome,
            &authority.source_closure_observation,
            &authority.coordinator_closure_observation,
        )?;

        let generation_is_current =
            authority.expected_recovery_generation == self.recovery_generation;
        let generation_is_replay = authority
            .expected_recovery_generation
            .checked_add(1)
            .is_some_and(|generation| generation == self.recovery_generation);
        let existing = self.after_effect_reconciliation.as_ref();
        let candidate_matches_existing = existing.is_some_and(|existing| {
            existing.recovery_inventory == authority.recovery_inventory
                && existing.source_closure_observation == authority.source_closure_observation
                && existing.coordinator_closure_observation
                    == authority.coordinator_closure_observation
                && existing.execution_epoch == authority.execution_epoch
        });
        if candidate_matches_existing {
            if (generation_is_current || generation_is_replay)
                && existing
                    .is_some_and(|state| state.recovery_generation == self.recovery_generation)
            {
                return Ok(());
            }
        }
        if !generation_is_current {
            return Err(
                WorkProgressError::AfterEffectReconciliationGenerationMismatch {
                    expected: self.recovery_generation,
                    actual: authority.expected_recovery_generation,
                },
            );
        }
        let next_recovery_generation = self
            .recovery_generation
            .checked_add(1)
            .ok_or(WorkProgressError::AfterEffectReconciliationGenerationOverflow)?;
        let previous_dispatch = self
            .after_effect_reconciliation
            .as_ref()
            .map(|state| state.recovery_inventory.dispatch_outcome())
            .or_else(|| {
                self.pre_publish_cancellation
                    .as_ref()
                    .and_then(PrePublishCancellationObservation::dispatch_outcome)
            });
        let dispatch_replays_previous =
            previous_dispatch.is_some_and(|previous| previous == dispatch_outcome);
        if !dispatch_replays_previous
            && dispatch_outcome.recovery_generation() != next_recovery_generation
        {
            return Err(
                WorkProgressError::AfterEffectReconciliationDispatchGenerationMismatch {
                    expected: next_recovery_generation,
                    actual: dispatch_outcome.recovery_generation(),
                },
            );
        }
        if let Some(previous_dispatch) = previous_dispatch {
            if !dispatch_replays_previous {
                dispatch_outcome
                    .validate_transition(previous_dispatch, self.execution_epoch)
                    .map_err(WorkProgressError::CancellationDispatchOutcome)?;
            }
        }
        if let Some(previous) = existing {
            if previous.source_closure_observation() != &authority.source_closure_observation {
                validate_after_effect_source_transition(
                    previous.source_closure_observation(),
                    &authority.source_closure_observation,
                )?;
            }
            if previous.coordinator_closure_observation()
                != &authority.coordinator_closure_observation
            {
                validate_after_effect_coordinator_transition(
                    previous.coordinator_closure_observation(),
                    &authority.coordinator_closure_observation,
                )?;
            }
        }

        // The request is intentionally read above to make the cancellation hold explicit.  Keep
        // the value live through validation so a future owner integration cannot accidentally
        // turn this method into a generic post-operation state setter.
        let _ = cancellation_request;
        self.after_effect_reconciliation = Some(AfterEffectReconciliationObservation::new(
            authority.recovery_inventory,
            authority.source_closure_observation,
            authority.coordinator_closure_observation,
            authority.execution_epoch,
            next_recovery_generation,
        ));
        self.recovery_generation = next_recovery_generation;
        Ok(())
    }

    /// Retain exact resource and namespace cleanup observations for the current accepted
    /// after-effect state without granting cleanup, adoption, claim, or terminal authority.
    #[allow(dead_code)]
    fn accept_cancellation_cleanup_observations(
        &mut self,
        authority: VerifiedCancellationCleanupObservations,
    ) -> Result<(), WorkProgressError> {
        if authority.operation_id != self.operation_id || authority.plan != self.plan {
            return Err(WorkProgressError::CancellationCleanupAuthorityMismatch);
        }
        if authority.execution_epoch != self.execution_epoch {
            return Err(WorkProgressError::CancellationCleanupEpochMismatch {
                expected: self.execution_epoch,
                actual: authority.execution_epoch,
            });
        }
        if self.disposition.is_terminal() {
            return Err(WorkProgressError::TerminalTransitionForbidden);
        }
        if self.active_audit.is_some() || self.disposition == FullProgressDisposition::AuditRequired
        {
            return Err(WorkProgressError::CancellationCleanupAuditActive);
        }
        if self.cancellation_request.is_none() {
            return Err(WorkProgressError::CancellationNotRequested);
        }
        let Some(current_inventory) = self.cancellation_inventory.as_ref() else {
            return Err(WorkProgressError::CancellationInventoryMissing);
        };
        let expected_disposition = match current_inventory.route() {
            CancellationRoute::BeforePublish => {
                FullProgressDisposition::CancelRequestedBeforePublish
            }
            CancellationRoute::AfterPublish => FullProgressDisposition::CancelRequestedAfterPublish,
            CancellationRoute::AfterObservedEffect => {
                FullProgressDisposition::CancelRequestedAfterObservedEffect
            }
        };
        if self.disposition != expected_disposition {
            return Err(WorkProgressError::CancellationCleanupStateInvalid);
        }
        if authority.cancellation_inventory != *current_inventory {
            return Err(WorkProgressError::CancellationCleanupInventoryMismatch);
        }
        let Some(admission_claims) = self.admission_claims.as_ref() else {
            return Err(WorkProgressError::CancellationAdmissionClaimsMissing);
        };
        authority.cancellation_inventory.validate_against(
            self.operation_id,
            &self.plan,
            &self.steps,
            self.execution_epoch,
            admission_claims,
        )?;

        let Some(current_after_effect) = self.after_effect_reconciliation.as_ref() else {
            return Err(WorkProgressError::CancellationCleanupAfterEffectMissing);
        };
        if authority.after_effect_reconciliation != *current_after_effect {
            return Err(WorkProgressError::CancellationCleanupAfterEffectMismatch);
        }
        if authority.after_effect_reconciliation.execution_epoch() != self.execution_epoch {
            return Err(
                WorkProgressError::CancellationCleanupAfterEffectEpochMismatch {
                    expected: self.execution_epoch,
                    actual: authority.after_effect_reconciliation.execution_epoch(),
                },
            );
        }

        let dispatch_outcome = authority
            .after_effect_reconciliation
            .recovery_inventory()
            .dispatch_outcome();
        let recovery_inventory = authority.after_effect_reconciliation.recovery_inventory();
        let cleanup_manifest =
            self.cancellation_cleanup_manifest(recovery_inventory, dispatch_outcome)?;
        self.validate_cancellation_resource_cleanup_observation_record(
            &authority.resource_cleanup_observation,
            &cleanup_manifest,
            recovery_inventory,
            dispatch_outcome,
        )?;
        self.validate_cancellation_namespace_cleanup_observation_record(
            &authority.namespace_cleanup_observation,
            &cleanup_manifest,
            recovery_inventory,
            dispatch_outcome,
        )?;
        if authority.resource_cleanup_observation.observation_epoch() != self.execution_epoch {
            return Err(
                WorkProgressError::CancellationCleanupResourceEpochMismatch {
                    expected: self.execution_epoch,
                    actual: authority.resource_cleanup_observation.observation_epoch(),
                },
            );
        }
        if authority.namespace_cleanup_observation.observation_epoch() != self.execution_epoch {
            return Err(
                WorkProgressError::CancellationCleanupNamespaceEpochMismatch {
                    expected: self.execution_epoch,
                    actual: authority.namespace_cleanup_observation.observation_epoch(),
                },
            );
        }
        let generation_is_current =
            authority.expected_recovery_generation == self.recovery_generation;
        let generation_is_replay = authority
            .expected_recovery_generation
            .checked_add(1)
            .is_some_and(|generation| generation == self.recovery_generation);
        let existing = self.cancellation_cleanup_observations.as_ref();
        if existing.is_none() {
            validate_resource_cleanup_initial_contexts(
                &authority.resource_cleanup_observation,
                self.execution_epoch,
            )?;
            validate_namespace_cleanup_initial_contexts(
                &authority.namespace_cleanup_observation,
                self.execution_epoch,
            )?;
        }
        let candidate_matches_existing = existing.is_some_and(|existing| {
            existing.resource_cleanup_observation == authority.resource_cleanup_observation
                && existing.namespace_cleanup_observation == authority.namespace_cleanup_observation
                && existing.after_effect_generation
                    == authority.after_effect_reconciliation.recovery_generation()
                && existing.execution_epoch == authority.execution_epoch
        });
        if candidate_matches_existing {
            if (generation_is_current || generation_is_replay)
                && existing
                    .is_some_and(|state| state.recovery_generation == self.recovery_generation)
            {
                return Ok(());
            }
            return Err(WorkProgressError::CancellationCleanupGenerationMismatch {
                expected: self.recovery_generation,
                actual: authority.expected_recovery_generation,
            });
        }
        if !generation_is_current {
            return Err(WorkProgressError::CancellationCleanupGenerationMismatch {
                expected: self.recovery_generation,
                actual: authority.expected_recovery_generation,
            });
        }
        let next_recovery_generation = self
            .recovery_generation
            .checked_add(1)
            .ok_or(WorkProgressError::CancellationCleanupGenerationOverflow)?;

        if let Some(previous) = existing {
            let dispatch_changed = !cancellation_dispatch_effects_equal(
                previous
                    .resource_cleanup_observation
                    .manifest()
                    .dispatch_outcome(),
                dispatch_outcome,
            );
            validate_resource_cleanup_transition(
                &previous.resource_cleanup_observation,
                &authority.resource_cleanup_observation,
                self.execution_epoch,
                dispatch_changed,
            )?;
            validate_namespace_cleanup_transition(
                &previous.namespace_cleanup_observation,
                &authority.namespace_cleanup_observation,
                self.execution_epoch,
                dispatch_changed,
            )?;
        }

        self.cancellation_cleanup_observations = Some(CancellationCleanupObservation::new(
            authority.resource_cleanup_observation,
            authority.namespace_cleanup_observation,
            authority.after_effect_reconciliation.recovery_generation(),
            authority.execution_epoch,
            next_recovery_generation,
        ));
        self.recovery_generation = next_recovery_generation;
        Ok(())
    }

    /// Return the bounded recovery-observation generation.
    pub const fn recovery_generation(&self) -> u64 {
        self.recovery_generation
    }

    /// Borrow the retained prepublication cancellation observation, when present.
    pub fn pre_publish_cancellation(&self) -> Option<&PrePublishCancellationObservation> {
        self.pre_publish_cancellation.as_ref()
    }

    /// Borrow the accepted nonterminal after-effect reconciliation state, when present.
    pub fn after_effect_reconciliation(&self) -> Option<&AfterEffectReconciliationObservation> {
        self.after_effect_reconciliation.as_ref()
    }

    /// Borrow the accepted nonterminal cleanup observations, when present.
    pub fn cancellation_cleanup_observations(&self) -> Option<&CancellationCleanupObservation> {
        self.cancellation_cleanup_observations.as_ref()
    }

    /// Record a bounded retry disposition without changing any step completion.
    pub(crate) fn request_retry(
        &mut self,
        observation: RetryObservation,
    ) -> Result<(), WorkProgressError> {
        observation.validate()?;
        if observation.lease_expiry_unix_ms() <= observation.next_retry_unix_ms() {
            return Err(WorkProgressError::EmptyRetryWindow);
        }
        if self.disposition.is_terminal() {
            return Err(WorkProgressError::TerminalTransitionForbidden);
        }
        if matches!(
            self.disposition,
            FullProgressDisposition::AuditRequired
                | FullProgressDisposition::CancelRequestedBeforePublish
                | FullProgressDisposition::CancelRequestedAfterPublish
                | FullProgressDisposition::CancelRequestedAfterObservedEffect
        ) {
            return Err(WorkProgressError::ProgressSuspended {
                disposition: self.disposition,
            });
        }
        if let Some(existing) = &self.retry_observation {
            if existing == &observation {
                return Ok(());
            }
            if observation.attempt() <= existing.attempt() {
                return Err(WorkProgressError::RetryReplayConflict {
                    attempt: observation.attempt(),
                });
            }
        }
        if let Some(last) = &self.last_retry_observation {
            if last == &observation {
                return Ok(());
            }
            if observation.attempt() <= last.attempt() {
                return Err(WorkProgressError::RetryReplayConflict {
                    attempt: observation.attempt(),
                });
            }
        }
        self.last_retry_observation = Some(observation.clone());
        self.disposition = FullProgressDisposition::RetryPending;
        self.retry_observation = Some(observation);
        Ok(())
    }

    /// Resume a retry only after a private owner has reacquired its lease in the valid window.
    #[allow(dead_code)]
    fn resume_retry(&mut self, authority: VerifiedRetryResume) -> Result<(), WorkProgressError> {
        if self.disposition != FullProgressDisposition::RetryPending {
            return Err(WorkProgressError::RetryResumeNotPending);
        }
        let Some(observation) = &self.retry_observation else {
            return Err(WorkProgressError::RetryResumeNotPending);
        };
        if authority.operation_id != self.operation_id
            || authority.plan != self.plan
            || authority.retry_observation != *observation
        {
            return Err(WorkProgressError::RetryResumeAuthorityMismatch);
        }
        if authority.expected_execution_epoch != self.execution_epoch {
            return Err(WorkProgressError::RetryResumeEpochMismatch {
                expected: self.execution_epoch,
                actual: authority.expected_execution_epoch,
            });
        }
        if authority.now_unix_ms < observation.next_retry_unix_ms()
            || authority.now_unix_ms >= observation.lease_expiry_unix_ms()
        {
            return Err(WorkProgressError::RetryResumeWindowInvalid {
                now: authority.now_unix_ms,
                next_retry: observation.next_retry_unix_ms(),
                lease_expiry: observation.lease_expiry_unix_ms(),
            });
        }
        self.execution_epoch = self
            .execution_epoch
            .checked_add(1)
            .ok_or(WorkProgressError::ExecutionEpochOverflow)?;
        self.retry_observation = None;
        self.disposition = FullProgressDisposition::None;
        Ok(())
    }

    /// Rebind every scope for one stable source after a private owner reacquires its lease.
    ///
    /// Reacquisition only advances the owner epoch and observed scopes.  It never changes the
    /// frozen command or plan, marks a step complete, clears a retry/audit/cancellation
    /// disposition, or grants a live source capability.
    #[allow(dead_code)]
    fn reacquire_source(
        &mut self,
        authority: VerifiedSourceReacquisition,
    ) -> Result<(), WorkProgressError> {
        if self.disposition.is_terminal() {
            return Err(WorkProgressError::TerminalTransitionForbidden);
        }
        if authority.operation_id != self.operation_id || authority.plan != self.plan {
            return Err(WorkProgressError::SourceReacquisitionAuthorityMismatch);
        }
        let Some(current_scope) = self
            .scopes
            .iter()
            .find(|scope| scope.participant_id == authority.target_participant_id)
        else {
            return Err(WorkProgressError::SourceReacquisitionTargetInvalid {
                participant_id: authority.target_participant_id,
            });
        };
        if current_scope.kind != ParticipantKind::SourceDatabase
            || current_scope.source_fence.is_none()
        {
            return Err(WorkProgressError::SourceReacquisitionTargetInvalid {
                participant_id: authority.target_participant_id,
            });
        }
        if current_scope != &authority.old_scope {
            return Err(WorkProgressError::SourceReacquisitionAuthorityMismatch);
        }
        if authority.execution_epoch != self.execution_epoch {
            return Err(WorkProgressError::SourceReacquisitionEpochMismatch {
                expected: self.execution_epoch,
                actual: authority.execution_epoch,
            });
        }
        authority.new_fence.validate()?;
        let old_fence = current_scope
            .source_fence
            .as_ref()
            .expect("source database scope checked above")
            .clone();
        if !same_stable_source_identity(&old_fence, &authority.new_fence) {
            return Err(WorkProgressError::SourceReacquisitionIdentityMismatch);
        }
        let current_lease_epoch = self
            .scopes
            .iter()
            .filter_map(|scope| scope.source_fence.as_ref())
            .filter(|fence| same_stable_source_identity(fence, &old_fence))
            .map(StoredSourceFence::lease_epoch)
            .max()
            .unwrap_or(old_fence.lease_epoch());
        if authority.new_fence.lease_epoch() <= current_lease_epoch {
            return Err(WorkProgressError::SourceReacquisitionLeaseRegressed {
                observed: authority.new_fence.lease_epoch(),
                current: current_lease_epoch,
            });
        }
        let minimum_revision = self.minimum_source_revision_for_stable_source(&old_fence);
        if authority.new_fence.source_revision() < minimum_revision {
            return Err(WorkProgressError::SourceReacquisitionRevisionRegressed {
                observed: authority.new_fence.source_revision(),
                minimum: minimum_revision,
            });
        }
        let next_execution_epoch = self
            .execution_epoch
            .checked_add(1)
            .ok_or(WorkProgressError::SourceReacquisitionEpochOverflow)?;
        for scope in &mut self.scopes {
            if scope
                .source_fence
                .as_ref()
                .is_some_and(|fence| same_stable_source_identity(fence, &old_fence))
            {
                scope.source_fence = Some(authority.new_fence.clone());
            }
        }
        self.execution_epoch = next_execution_epoch;
        Ok(())
    }

    /// Record a bounded audit disposition while preserving all current step observations.
    pub(crate) fn require_audit(
        &mut self,
        observation: ProgressAuditObservation,
    ) -> Result<(), WorkProgressError> {
        if self.disposition.is_terminal() {
            return Err(WorkProgressError::TerminalTransitionForbidden);
        }
        observation.validate_for_plan(&self.plan)?;
        if let Some(active) = &self.active_audit {
            if active.observation == observation {
                return Ok(());
            }
            return Err(WorkProgressError::AuditReplayConflict);
        }
        if self.disposition == FullProgressDisposition::AuditRequired {
            return Err(WorkProgressError::AuditStateMissing);
        }
        let observed_effect_cancellation = self.disposition
            == FullProgressDisposition::CancelRequestedAfterObservedEffect
            && self
                .cancellation_inventory
                .as_ref()
                .is_some_and(|inventory| {
                    inventory.route() == CancellationRoute::AfterObservedEffect
                });
        if self.phase.is_pre_publication() && !observed_effect_cancellation {
            return Err(WorkProgressError::AuditPhaseInvalid);
        }
        let suspended_hold = match self.disposition {
            FullProgressDisposition::None => AuditSuspendedHold::None,
            FullProgressDisposition::RetryPending => AuditSuspendedHold::RetryPending,
            FullProgressDisposition::CancelRequestedBeforePublish => {
                AuditSuspendedHold::CancelRequestedBeforePublish
            }
            FullProgressDisposition::CancelRequestedAfterPublish => {
                AuditSuspendedHold::CancelRequestedAfterPublish
            }
            FullProgressDisposition::CancelRequestedAfterObservedEffect => {
                AuditSuspendedHold::CancelRequestedAfterObservedEffect
            }
            disposition => {
                return Err(WorkProgressError::AuditUnderlyingDispositionUnsupported {
                    disposition,
                });
            }
        };
        let generation = self
            .audit_generation
            .checked_add(1)
            .ok_or(WorkProgressError::AuditGenerationOverflow)?;
        self.active_audit = Some(ActiveAudit {
            generation,
            observation,
            suspended_hold,
        });
        self.audit_generation = generation;
        self.disposition = FullProgressDisposition::AuditRequired;
        Ok(())
    }

    /// Resolve one active audit only through a private authority matching its exact hold.
    #[allow(dead_code)]
    fn resolve_audit(
        &mut self,
        authority: VerifiedAuditResolution,
    ) -> Result<(), WorkProgressError> {
        if self.disposition != FullProgressDisposition::AuditRequired {
            return Err(WorkProgressError::AuditResolutionNotActive);
        }
        let Some(active) = &self.active_audit else {
            return Err(WorkProgressError::AuditStateMissing);
        };
        if authority.operation_id != self.operation_id || authority.plan != self.plan {
            return Err(WorkProgressError::AuditResolutionAuthorityMismatch);
        }
        if authority.execution_epoch != self.execution_epoch {
            return Err(WorkProgressError::AuditResolutionEpochMismatch {
                expected: self.execution_epoch,
                actual: authority.execution_epoch,
            });
        }
        if authority.generation != active.generation {
            return Err(WorkProgressError::AuditResolutionGenerationMismatch {
                expected: active.generation,
                actual: authority.generation,
            });
        }
        if authority.observation != active.observation {
            return Err(WorkProgressError::AuditResolutionObservationMismatch);
        }
        let restored_disposition = match active.suspended_hold {
            AuditSuspendedHold::None => FullProgressDisposition::None,
            AuditSuspendedHold::RetryPending if self.retry_observation.is_some() => {
                FullProgressDisposition::RetryPending
            }
            AuditSuspendedHold::CancelRequestedBeforePublish
                if self.cancellation_request.is_some() =>
            {
                FullProgressDisposition::CancelRequestedBeforePublish
            }
            AuditSuspendedHold::CancelRequestedAfterPublish
                if self.cancellation_request.is_some() =>
            {
                FullProgressDisposition::CancelRequestedAfterPublish
            }
            AuditSuspendedHold::CancelRequestedAfterObservedEffect
                if self.cancellation_request.is_some() =>
            {
                FullProgressDisposition::CancelRequestedAfterObservedEffect
            }
            _ => return Err(WorkProgressError::AuditSuspendedHoldMissing),
        };
        self.active_audit = None;
        self.disposition = restored_disposition;
        Ok(())
    }

    /// Record cancellation as a resumable disposition; no terminal result is emitted here.
    pub(crate) fn request_cancellation(
        &mut self,
        observation: CancellationRequestObservation,
    ) -> Result<(), WorkProgressError> {
        observation.validate()?;
        if self.disposition.is_terminal() {
            return Err(WorkProgressError::TerminalTransitionForbidden);
        }
        if let Some(existing) = self.cancellation_request {
            if existing == observation {
                return if self.cancellation_inventory.is_some() {
                    Ok(())
                } else {
                    Err(WorkProgressError::CancellationInventoryMissing)
                };
            }
            return Err(WorkProgressError::CancellationReplayConflict);
        }

        let admission_claims = self
            .admission_claims
            .as_ref()
            .ok_or(WorkProgressError::CancellationAdmissionClaimsMissing)?;
        let generation = self
            .cancellation_inventory
            .as_ref()
            .map(CancellationInventory::generation)
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(WorkProgressError::CancellationGenerationOverflow)?;
        let inventory = CancellationInventory::capture(
            self.operation_id,
            &self.plan,
            self.execution_epoch,
            generation,
            &self.steps,
            admission_claims,
        )?;
        let route = inventory.route();
        let requested_disposition = match route {
            CancellationRoute::BeforePublish => {
                FullProgressDisposition::CancelRequestedBeforePublish
            }
            CancellationRoute::AfterPublish => FullProgressDisposition::CancelRequestedAfterPublish,
            CancellationRoute::AfterObservedEffect => {
                FullProgressDisposition::CancelRequestedAfterObservedEffect
            }
        };
        if self.disposition == FullProgressDisposition::AuditRequired {
            if self.active_audit.is_none() {
                return Err(WorkProgressError::AuditStateMissing);
            }
            if route == CancellationRoute::BeforePublish {
                return Err(WorkProgressError::CancellationPhaseInvalid);
            }
            self.cancellation_request = Some(observation);
            self.cancellation_inventory = Some(inventory);
            if let Some(active) = &mut self.active_audit {
                active.suspended_hold = match requested_disposition {
                    FullProgressDisposition::CancelRequestedAfterPublish => {
                        AuditSuspendedHold::CancelRequestedAfterPublish
                    }
                    FullProgressDisposition::CancelRequestedAfterObservedEffect => {
                        AuditSuspendedHold::CancelRequestedAfterObservedEffect
                    }
                    _ => return Err(WorkProgressError::CancellationPhaseInvalid),
                };
            }
            return Ok(());
        }
        self.cancellation_request = Some(observation);
        self.cancellation_inventory = Some(inventory);
        self.disposition = requested_disposition;
        Ok(())
    }

    /// Apply one private, current-epoch exact-subset readiness deferral.
    #[allow(dead_code)]
    pub(crate) fn apply_verified_readiness_deferral(
        &mut self,
        authority: VerifiedReadinessDeferral,
    ) -> Result<ProgressApplyOutcome, WorkProgressError> {
        if authority.operation_id != self.operation_id || authority.plan != self.plan {
            return Err(WorkProgressError::ReadinessDeferralAuthorityMismatch);
        }
        if authority.execution_epoch != self.execution_epoch {
            return Err(WorkProgressError::ReadinessDeferralEpochMismatch {
                expected: self.execution_epoch,
                actual: authority.execution_epoch,
            });
        }
        let selected_artifact_keys = authority.observation.manifest().artifact_keys();
        let expected_manifest = self.readiness_deferral_manifest(selected_artifact_keys)?;
        authority
            .observation
            .validate_against(&expected_manifest)
            .map_err(WorkProgressError::ReadinessSchedulingObservation)?;
        if authority.observation.observation_epoch() != self.execution_epoch {
            return Err(WorkProgressError::ReadinessDeferralCandidateEpochMismatch);
        }
        let expected_generation = authority
            .expected_previous_generation
            .checked_add(1)
            .ok_or(WorkProgressError::ReadinessDeferralGenerationOverflow)?;
        if authority.observation.observation_generation() != expected_generation {
            return Err(WorkProgressError::ReadinessDeferralGenerationMismatch {
                expected: expected_generation,
                actual: authority.observation.observation_generation(),
            });
        }
        let compact = ReadinessDeferralBatchObservation::new(
            authority.observation.observation_generation(),
            authority.observation.observation_epoch(),
            selected_artifact_keys.to_vec(),
            authority.observation.rows().to_vec(),
        )
        .map_err(WorkProgressError::ReadinessSchedulingObservation)?;
        if let Some(existing) = self
            .readiness_deferral_batches
            .iter()
            .find(|batch| batch.observation_generation() == compact.observation_generation())
        {
            if existing == &compact {
                return Ok(ProgressApplyOutcome::Replay);
            }
            return Err(WorkProgressError::ReadinessDeferralPublicationConflict {
                key: compact.rows().first().map_or(
                    selected_artifact_keys[0],
                    ReadinessSchedulingObservationRow::readiness_key,
                ),
            });
        }
        let current_previous_generation = self
            .readiness_deferral_batches
            .last()
            .map_or(0, ReadinessDeferralBatchObservation::observation_generation);
        if authority.expected_previous_generation != current_previous_generation {
            return Err(WorkProgressError::ReadinessDeferralGenerationMismatch {
                expected: current_previous_generation,
                actual: authority.expected_previous_generation,
            });
        }
        self.validate_completion_history()?;
        if self.disposition != FullProgressDisposition::None {
            return Err(WorkProgressError::ProgressSuspended {
                disposition: self.disposition,
            });
        }
        self.validate_readiness_scheduling_observation_record(
            &authority.observation,
            selected_artifact_keys,
        )?;
        for artifact in selected_artifact_keys {
            let step = self
                .steps
                .iter()
                .find(|step| step.key == *artifact)
                .ok_or(WorkProgressError::UnknownWorkUnit { key: *artifact })?;
            if step.key.role() != WorkUnitRole::Artifact
                || step.requirement != ParticipantRequirement::OptionalRebuildable
                || step.status != ProgressStepStatus::Pending
            {
                return Err(WorkProgressError::ReadinessDeferralArtifactNotPending {
                    key: *artifact,
                });
            }
        }
        for row in authority.observation.rows() {
            let step = self
                .steps
                .iter()
                .find(|step| step.key == row.readiness_key())
                .ok_or(WorkProgressError::UnknownWorkUnit {
                    key: row.readiness_key(),
                })?;
            if !matches!(
                (
                    step.status,
                    step.evidence.as_ref(),
                    step.completion_context.as_ref()
                ),
                (
                    ProgressStepStatus::Completed,
                    Some(StoredStepEvidence::ReadinessScheduled { .. }),
                    Some(_)
                )
            ) {
                return Err(WorkProgressError::ReadinessDeferralReadinessIncomplete {
                    key: row.readiness_key(),
                });
            }
        }
        let previous_rows = self
            .readiness_deferral_batches
            .last()
            .map(|batch| batch.rows())
            .unwrap_or(&[]);
        for row in authority.observation.rows() {
            let previous = previous_rows
                .iter()
                .find(|previous| previous.readiness_key() == row.readiness_key());
            let normal = self
                .steps
                .iter()
                .find(|step| step.key == row.readiness_key())
                .and_then(|step| step.evidence.as_ref())
                .and_then(|evidence| match evidence {
                    StoredStepEvidence::ReadinessScheduled { observation } => Some(observation),
                    _ => None,
                });
            validate_readiness_publication_order(normal, previous, row)?;
        }
        self.readiness_deferral_batches.push(compact);
        for artifact in selected_artifact_keys {
            let step = self
                .steps
                .iter_mut()
                .find(|step| step.key == *artifact)
                .expect("validated optional artifact key");
            step.status = ProgressStepStatus::Deferred;
        }
        self.refresh_derived_state();
        Ok(ProgressApplyOutcome::Applied)
    }

    /// Apply a crate-private completion after checking its full operation and prerequisite fence.
    pub(crate) fn apply_verified_completion(
        &mut self,
        completion: VerifiedStepCompletion,
    ) -> Result<ProgressApplyOutcome, WorkProgressError> {
        if completion.operation_id != self.operation_id {
            return Err(WorkProgressError::OperationIdMismatch);
        }
        if completion.plan != self.plan {
            return Err(WorkProgressError::PlanMismatch);
        }
        completion.evidence.validate()?;
        let index = self
            .steps
            .iter()
            .position(|step| step.key == completion.key)
            .ok_or(WorkProgressError::UnknownWorkUnit {
                key: completion.key,
            })?;
        self.validate_completion_history()?;
        let current_status = self.steps[index].status;
        if current_status == ProgressStepStatus::Completed {
            let accepted_context = self.steps[index].completion_context.as_ref().ok_or(
                WorkProgressError::CompletionContextMissing {
                    key: completion.key,
                },
            )?;
            let accepted_evidence = self.steps[index].evidence.as_ref().ok_or(
                WorkProgressError::CompletionEvidenceMissing {
                    key: completion.key,
                },
            )?;
            let replay_context =
                CompletionContext::new(completion.execution_epoch, completion.scope.clone());
            if accepted_context == &replay_context && accepted_evidence == &completion.evidence {
                return Ok(ProgressApplyOutcome::Replay);
            }
            return Err(WorkProgressError::CompletionReplayConflict {
                key: completion.key,
            });
        }
        let expected_scope = self
            .scopes
            .iter()
            .find(|scope| scope.participant_id == completion.key.participant_id())
            .ok_or(WorkProgressError::ParticipantScopeMissing {
                participant_id: completion.key.participant_id(),
            })?;
        if expected_scope != &completion.scope
            || completion.scope.participant_id != completion.key.participant_id()
        {
            return Err(WorkProgressError::ParticipantScopeMismatch {
                key: completion.key,
            });
        }
        if current_status != ProgressStepStatus::Pending {
            return Err(WorkProgressError::StepNotPending {
                key: completion.key,
            });
        }
        if self.disposition == FullProgressDisposition::RetryPending {
            return Err(WorkProgressError::ProgressSuspended {
                disposition: self.disposition,
            });
        }
        if self.disposition != FullProgressDisposition::None
            && self.disposition != FullProgressDisposition::RetryPending
        {
            return Err(WorkProgressError::ProgressSuspended {
                disposition: self.disposition,
            });
        }
        if completion.execution_epoch != self.execution_epoch {
            return Err(WorkProgressError::ExecutionEpochMismatch {
                expected: self.execution_epoch,
                actual: completion.execution_epoch,
            });
        }
        for edge in self
            .plan
            .prerequisites()
            .iter()
            .filter(|edge| edge.dependent() == completion.key)
        {
            let prerequisite = self
                .step_for(edge.prerequisite())
                .ok_or(WorkProgressError::PrerequisiteMissing { edge: *edge })?;
            if prerequisite.status != ProgressStepStatus::Completed
                && prerequisite.status != ProgressStepStatus::NotApplicable
            {
                return Err(WorkProgressError::PrerequisiteIncomplete {
                    prerequisite: prerequisite.key,
                    dependent: completion.key,
                });
            }
        }
        let minimum_revision = expected_scope
            .source_fence()
            .and_then(|fence| self.minimum_source_revision_for_fence(fence));
        let completion_context =
            CompletionContext::new(completion.execution_epoch, completion.scope.clone());
        validate_normal_completion_evidence(
            completion.key,
            &completion_context,
            &completion.evidence,
            minimum_revision,
        )?;
        self.steps[index].status = ProgressStepStatus::Completed;
        self.steps[index].evidence = Some(completion.evidence);
        self.steps[index].completion_context = Some(completion_context);
        if self.disposition == FullProgressDisposition::RetryPending {
            self.disposition = FullProgressDisposition::None;
            self.retry_observation = None;
        }
        self.refresh_derived_state();
        Ok(ProgressApplyOutcome::Applied)
    }

    /// Apply a private owner-issued settlement for every admitted claim.
    ///
    /// Normal success is withheld until all required work and optional artifact decisions are
    /// resolved and the exact admitted inventory has a non-held settlement outcome for every
    /// claim.  A valid inventory containing `Held` rows is retained as a nonterminal observation
    /// so a later owner authority can settle those rows.  This method has no live release or
    /// capacity authority.  Replaying the exact observation is idempotent, while a settled row
    /// cannot be rewritten.
    #[allow(dead_code)]
    pub(crate) fn apply_verified_claim_settlement(
        &mut self,
        authority: VerifiedClaimSettlement,
    ) -> Result<ProgressApplyOutcome, WorkProgressError> {
        if authority.operation_id != self.operation_id || authority.plan != self.plan {
            return Err(WorkProgressError::ClaimSettlementAuthorityMismatch);
        }
        if authority.execution_epoch != self.execution_epoch {
            return Err(WorkProgressError::ClaimSettlementEpochMismatch {
                expected: self.execution_epoch,
                actual: authority.execution_epoch,
            });
        }
        authority
            .admission_claims
            .validate()
            .map_err(WorkProgressError::ClaimSettlement)?;
        let Some(admission_claims) = self.admission_claims.as_ref() else {
            return Err(WorkProgressError::ClaimSettlementAdmissionClaimsMissing);
        };
        if admission_claims != &authority.admission_claims {
            return Err(WorkProgressError::ClaimSettlementClaimsMismatch);
        }
        authority
            .settlement
            .validate_normal_success_settlement(&authority.admission_claims)
            .map_err(WorkProgressError::ClaimSettlement)?;
        if let Some(existing) = &self.claim_settlement {
            if existing == &authority.settlement {
                return Ok(ProgressApplyOutcome::Replay);
            }
            for settled in existing.settlements() {
                if settled.outcome() == AdmissionClaimSettlementOutcome::Held {
                    continue;
                }
                let candidate = authority
                    .settlement
                    .settlements()
                    .iter()
                    .find(|candidate| candidate.claim_id() == settled.claim_id())
                    .expect("validated settlement coverage");
                if candidate.outcome() != settled.outcome() {
                    return Err(WorkProgressError::ClaimSettlementReplayConflict);
                }
            }
        }
        if self.disposition.is_terminal() {
            return Err(WorkProgressError::TerminalTransitionForbidden);
        }
        if self.disposition != FullProgressDisposition::None {
            return Err(WorkProgressError::ProgressSuspended {
                disposition: self.disposition,
            });
        }
        if !self.all_required_complete() || !self.optional_artifacts_resolved() {
            return Err(WorkProgressError::ClaimSettlementProgressIncomplete);
        }

        let all_settled_without_held = authority.settlement.all_settled_without_held();
        self.claim_settlement = Some(authority.settlement);
        if all_settled_without_held {
            self.phase = OperationPhase::Terminal;
            self.disposition = if self.has_deferred_optional_artifact() {
                FullProgressDisposition::SucceededWithDeferredArtifacts
            } else {
                FullProgressDisposition::Succeeded
            };
        }
        Ok(ProgressApplyOutcome::Applied)
    }

    /// Retain a private-owner prepublication cleanup observation and derive a safe terminal result
    /// only after every entered action and admitted claim is covered.
    #[allow(dead_code)]
    fn finish_pre_publish_cancellation(
        &mut self,
        authority: VerifiedPrePublishCancellation,
    ) -> Result<(), WorkProgressError> {
        if authority.operation_id != self.operation_id || authority.plan != self.plan {
            return Err(WorkProgressError::PrePublishCancellationAuthorityMismatch);
        }
        if self.after_effect_reconciliation.is_some() {
            return Err(WorkProgressError::PrePublishCancellationAfterEffectReconciliationActive);
        }
        if authority.execution_epoch != self.execution_epoch {
            return Err(WorkProgressError::PrePublishCancellationEpochMismatch {
                expected: self.execution_epoch,
                actual: authority.execution_epoch,
            });
        }
        if authority.dispatch_outcome.observation_epoch() != self.execution_epoch {
            return Err(
                WorkProgressError::PrePublishCancellationDispatchEpochMismatch {
                    expected: self.execution_epoch,
                    actual: authority.dispatch_outcome.observation_epoch(),
                },
            );
        }
        let generation_is_current =
            authority.expected_recovery_generation == self.recovery_generation;
        let generation_is_replay = authority
            .expected_recovery_generation
            .checked_add(1)
            .is_some_and(|generation| generation == self.recovery_generation);
        let Some(admission_claims) = self.admission_claims.as_ref() else {
            return Err(WorkProgressError::PrePublishCancellationAdmissionClaimsMissing);
        };
        let Some(current_inventory) = self.cancellation_inventory.as_ref() else {
            return Err(WorkProgressError::CancellationInventoryMissing);
        };
        authority.cancellation_inventory.validate_against(
            self.operation_id,
            &self.plan,
            &self.steps,
            self.execution_epoch,
            admission_claims,
        )?;
        if &authority.cancellation_inventory != current_inventory {
            return Err(WorkProgressError::PrePublishCancellationInventoryMismatch);
        }
        if authority.cancellation_inventory.route() != CancellationRoute::BeforePublish {
            return Err(WorkProgressError::PrePublishCancellationRouteInvalid {
                route: authority.cancellation_inventory.route(),
            });
        }
        if self.active_audit.is_some() || self.disposition == FullProgressDisposition::AuditRequired
        {
            return Err(WorkProgressError::PrePublishCancellationAuditActive);
        }
        if self.disposition != FullProgressDisposition::CancelRequestedBeforePublish
            && self.disposition != FullProgressDisposition::CancelledBeforePublish
        {
            return Err(WorkProgressError::PrePublishCancellationStateInvalid);
        }

        self.validate_cancellation_dispatch_record(&authority.dispatch_outcome)?;
        let next_recovery_generation = self
            .recovery_generation
            .checked_add(1)
            .ok_or(WorkProgressError::PrePublishCancellationRecoveryGenerationOverflow)?;
        let dispatch_complete = authority
            .dispatch_outcome
            .rows()
            .iter()
            .all(|row| matches!(row.outcome(), CancellationDispatchOutcome::NeverEntered));

        authority
            .cleanup
            .validate()
            .map_err(WorkProgressError::PrePublishCleanup)?;
        authority
            .unstarted_quiescence
            .validate()
            .map_err(WorkProgressError::PrePublishUnstartedQuiescence)?;
        let uncheckpointed_first_effect_keys =
            self.uncheckpointed_first_effect_keys(&authority.cancellation_inventory);
        let unstarted_quiescence_complete = self.validate_pre_publish_unstarted_quiescence(
            &authority.cancellation_inventory,
            &authority.unstarted_quiescence,
            &uncheckpointed_first_effect_keys,
            admission_claims,
        )?;
        let cleanup_complete = self.validate_pre_publish_cleanup(
            &authority.cancellation_inventory,
            &authority.cleanup,
            admission_claims,
            unstarted_quiescence_complete && dispatch_complete,
        )?;
        self.validate_pre_publish_settlement(
            &authority.cancellation_inventory,
            admission_claims,
            &authority.cleanup,
            &authority.settlement,
            unstarted_quiescence_complete,
            dispatch_complete,
        )?;

        if let Some(existing) = &self.pre_publish_cancellation {
            let dispatch_matches = existing
                .dispatch_outcome()
                .is_some_and(|stored| stored == &authority.dispatch_outcome);
            if existing.cleanup() == &authority.cleanup
                && existing.unstarted_quiescence() == &authority.unstarted_quiescence
                && existing.settlement() == &authority.settlement
                && dispatch_matches
            {
                if generation_is_current || generation_is_replay {
                    return Ok(());
                }
            }
            if !generation_is_current {
                return Err(
                    WorkProgressError::PrePublishCancellationRecoveryGenerationMismatch {
                        expected: self.recovery_generation,
                        actual: authority.expected_recovery_generation,
                    },
                );
            }
            if !dispatch_matches {
                if authority.dispatch_outcome.recovery_generation() != next_recovery_generation {
                    return Err(
                        WorkProgressError::PrePublishCancellationDispatchGenerationMismatch {
                            expected: next_recovery_generation,
                            actual: authority.dispatch_outcome.recovery_generation(),
                        },
                    );
                }
                if let Some(previous) = existing.dispatch_outcome() {
                    authority
                        .dispatch_outcome
                        .validate_transition(previous, self.execution_epoch)
                        .map_err(WorkProgressError::CancellationDispatchOutcome)?;
                }
            }
            validate_pre_publish_unstarted_quiescence_transition(
                existing.unstarted_quiescence(),
                &authority.unstarted_quiescence,
            )?;
            validate_pre_publish_settlement_transition(
                existing.settlement(),
                &authority.settlement,
            )?;
            if self.disposition == FullProgressDisposition::CancelledBeforePublish {
                return Err(WorkProgressError::TerminalTransitionForbidden);
            }
        } else if !generation_is_current {
            return Err(
                WorkProgressError::PrePublishCancellationRecoveryGenerationMismatch {
                    expected: self.recovery_generation,
                    actual: authority.expected_recovery_generation,
                },
            );
        } else if self.disposition == FullProgressDisposition::CancelledBeforePublish {
            return Err(WorkProgressError::PrePublishCancellationStateInvalid);
        }
        if self.pre_publish_cancellation.is_none()
            && authority.dispatch_outcome.recovery_generation() != next_recovery_generation
        {
            return Err(
                WorkProgressError::PrePublishCancellationDispatchGenerationMismatch {
                    expected: next_recovery_generation,
                    actual: authority.dispatch_outcome.recovery_generation(),
                },
            );
        }
        let settled = authority
            .settlement
            .satisfies_prepublication_cancellation(admission_claims)
            .map_err(WorkProgressError::PrePublishCancellationSettlement)?;
        self.pre_publish_cancellation =
            Some(PrePublishCancellationObservation::with_dispatch_outcome(
                authority.cleanup,
                authority.unstarted_quiescence,
                authority.settlement,
                authority.dispatch_outcome,
            ));
        self.recovery_generation = next_recovery_generation;
        if cleanup_complete && dispatch_complete && unstarted_quiescence_complete && settled {
            self.phase = OperationPhase::Terminal;
            self.disposition = FullProgressDisposition::CancelledBeforePublish;
        }
        Ok(())
    }

    fn validate_pre_publish_cleanup(
        &self,
        inventory: &CancellationInventory,
        cleanup: &PrePublishCleanupObservation,
        admission_claims: &AdmissionClaimInventory,
        unstarted_quiescence_complete: bool,
    ) -> Result<bool, WorkProgressError> {
        let has_operation_recovery_claim = admission_claims.claims().iter().any(|claim| {
            claim.allocation_class() == FullCapacityAllocationClass::OperationRecoveryPayload
        });
        for row in cleanup.actions() {
            let Some(action) = inventory
                .actions()
                .iter()
                .find(|action| action.action_ordinal() == row.action_ordinal())
            else {
                return Err(WorkProgressError::PrePublishCleanupActionSetMismatch {
                    action_ordinal: row.action_ordinal(),
                });
            };
            if action.state() != CancellationActionState::CleanablePrePublish {
                return Err(WorkProgressError::PrePublishCleanupActionSetMismatch {
                    action_ordinal: row.action_ordinal(),
                });
            }
        }

        let mut complete = true;
        for action in inventory.actions() {
            let Some(row) = cleanup
                .actions()
                .iter()
                .find(|row| row.action_ordinal() == action.action_ordinal())
            else {
                if action.state() == CancellationActionState::CleanablePrePublish {
                    complete = false;
                }
                continue;
            };
            if action.state() != CancellationActionState::CleanablePrePublish {
                return Err(WorkProgressError::PrePublishCleanupActionSetMismatch {
                    action_ordinal: action.action_ordinal(),
                });
            }
            let Some(policy) = self
                .origin_policies
                .iter()
                .find(|policy| policy.action_ordinal == action.action_ordinal())
            else {
                return Err(WorkProgressError::PrePublishCleanupActionSetMismatch {
                    action_ordinal: action.action_ordinal(),
                });
            };

            match row.final_claim() {
                Some(PrePublishFinalClaimObservation::NoFinalClaim) => {}
                Some(PrePublishFinalClaimObservation::Uncertain) | None => complete = false,
            }
            match (policy.origin, row.original_state()) {
                (
                    PrePublishOriginPolicy::OriginalStatePreserved,
                    Some(PrePublishOriginalStateObservation::Preserved),
                )
                | (
                    PrePublishOriginPolicy::NoOriginalObject,
                    Some(PrePublishOriginalStateObservation::NoOriginalObject),
                ) => {}
                (_, Some(PrePublishOriginalStateObservation::Uncertain)) | (_, None) => {
                    complete = false
                }
                (
                    PrePublishOriginPolicy::OriginalStatePreserved,
                    Some(PrePublishOriginalStateObservation::NoOriginalObject),
                )
                | (
                    PrePublishOriginPolicy::NoOriginalObject,
                    Some(PrePublishOriginalStateObservation::Preserved),
                ) => {
                    return Err(WorkProgressError::PrePublishCleanupOriginalStateMismatch {
                        action_ordinal: action.action_ordinal(),
                    });
                }
            }

            let staging_required =
                action_has_required_role(&self.plan, action.action_ordinal(), WorkUnitRole::Stage);
            match (staging_required, row.staging_absence()) {
                (true, Some(PrePublishAbsenceObservation::Absent)) => {}
                (
                    true,
                    Some(
                        PrePublishAbsenceObservation::Preserved
                        | PrePublishAbsenceObservation::Uncertain,
                    ),
                )
                | (true, None) => complete = false,
                (false, None) => {}
                (false, Some(_)) => {
                    return Err(WorkProgressError::PrePublishCleanupStagingUnexpected {
                        action_ordinal: action.action_ordinal(),
                    });
                }
            }

            let recovery_required = has_operation_recovery_claim
                || action_has_required_role(
                    &self.plan,
                    action.action_ordinal(),
                    WorkUnitRole::RecoveryMaterialRetained,
                );
            match (recovery_required, row.operation_recovery_payload_absence()) {
                (true, Some(PrePublishAbsenceObservation::Absent)) => {}
                (
                    true,
                    Some(
                        PrePublishAbsenceObservation::Preserved
                        | PrePublishAbsenceObservation::Uncertain,
                    ),
                )
                | (true, None) => complete = false,
                (false, None) => {}
                (false, Some(_)) => {
                    return Err(WorkProgressError::PrePublishCleanupRecoveryUnexpected {
                        action_ordinal: action.action_ordinal(),
                    });
                }
            }
        }
        // A source read can be the last durable checkpoint before the first filesystem owner
        // enters its work.  The inventory still classifies that action as `NotStarted`, but a
        // pending non-read unit whose prerequisites are all closed is an ambiguous cancellation
        // boundary: cleanup must not claim that the operation-created namespace is absent until a
        // future owner establishes whether that first effect was entered.  Keep this boundary
        // nonterminal without adding a cleanup row for the still-unentered action.
        if !unstarted_quiescence_complete {
            complete = false;
        }
        Ok(complete)
    }

    fn uncheckpointed_first_effect_keys(
        &self,
        inventory: &CancellationInventory,
    ) -> BTreeSet<WorkUnitKey> {
        let mut blocked_dependents = BTreeSet::new();
        for edge in self.plan.prerequisites() {
            let prerequisite_ready = inventory
                .steps()
                .binary_search_by_key(&edge.prerequisite(), |step| step.key())
                .ok()
                .is_some_and(|index| {
                    matches!(
                        inventory.steps()[index].status(),
                        ProgressStepStatus::Completed | ProgressStepStatus::NotApplicable
                    )
                });
            if !prerequisite_ready {
                blocked_dependents.insert(edge.dependent());
            }
        }
        let mut eligible = BTreeSet::new();
        for action in inventory.actions() {
            if action.state() != CancellationActionState::NotStarted {
                continue;
            }
            for (index, unit) in self.plan.units().iter().enumerate() {
                if unit.key().action_ordinal() != action.action_ordinal()
                    || unit.applicability() != WorkUnitApplicability::Required
                    || unit.key().role() == WorkUnitRole::SourceRead
                {
                    continue;
                }
                let Some(snapshot) = inventory.steps().get(index) else {
                    continue;
                };
                if snapshot.status() == ProgressStepStatus::Pending
                    && !blocked_dependents.contains(&unit.key())
                {
                    eligible.insert(unit.key());
                }
            }
        }
        eligible
    }

    fn validate_pre_publish_unstarted_quiescence(
        &self,
        inventory: &CancellationInventory,
        quiescence: &PrePublishUnstartedQuiescenceObservation,
        eligible_keys: &BTreeSet<WorkUnitKey>,
        admission_claims: &AdmissionClaimInventory,
    ) -> Result<bool, WorkProgressError> {
        let mut expected_by_action = BTreeMap::<u8, Vec<WorkUnitKey>>::new();
        for key in eligible_keys {
            expected_by_action
                .entry(key.action_ordinal())
                .or_default()
                .push(*key);
        }
        let has_destination_staging_claim = admission_claims.claims().iter().any(|claim| {
            claim.allocation_class() == FullCapacityAllocationClass::DestinationStaging
        });
        let has_operation_recovery_claim = admission_claims.claims().iter().any(|claim| {
            claim.allocation_class() == FullCapacityAllocationClass::OperationRecoveryPayload
        });

        for row in quiescence.actions() {
            let Some(action) = inventory
                .actions()
                .iter()
                .find(|action| action.action_ordinal() == row.action_ordinal())
            else {
                return Err(
                    WorkProgressError::PrePublishUnstartedQuiescenceActionSetMismatch {
                        action_ordinal: row.action_ordinal(),
                    },
                );
            };
            if action.state() != CancellationActionState::NotStarted {
                return Err(
                    WorkProgressError::PrePublishUnstartedQuiescenceActionSetMismatch {
                        action_ordinal: row.action_ordinal(),
                    },
                );
            }
            let Some(expected_keys) = expected_by_action.get(&row.action_ordinal()) else {
                return Err(
                    WorkProgressError::PrePublishUnstartedQuiescenceActionSetMismatch {
                        action_ordinal: row.action_ordinal(),
                    },
                );
            };
            if row.eligible_pending_keys() != expected_keys.as_slice() {
                return Err(
                    WorkProgressError::PrePublishUnstartedQuiescenceKeySetMismatch {
                        action_ordinal: row.action_ordinal(),
                    },
                );
            }

            let staging_required = has_destination_staging_claim
                || action_has_required_role(&self.plan, row.action_ordinal(), WorkUnitRole::Stage);
            if !staging_required && row.staging_absence().is_some() {
                return Err(
                    WorkProgressError::PrePublishUnstartedQuiescenceStagingUnexpected {
                        action_ordinal: row.action_ordinal(),
                    },
                );
            }
            let recovery_required = has_operation_recovery_claim
                || action_has_required_role(
                    &self.plan,
                    row.action_ordinal(),
                    WorkUnitRole::RecoveryMaterialRetained,
                );
            if !recovery_required && row.operation_recovery_payload_absence().is_some() {
                return Err(
                    WorkProgressError::PrePublishUnstartedQuiescenceRecoveryUnexpected {
                        action_ordinal: row.action_ordinal(),
                    },
                );
            }
        }

        let mut complete = true;
        for (action_ordinal, expected_keys) in &expected_by_action {
            let Some(_action) = inventory
                .actions()
                .iter()
                .find(|action| action.action_ordinal() == *action_ordinal)
            else {
                return Err(
                    WorkProgressError::PrePublishUnstartedQuiescenceActionSetMismatch {
                        action_ordinal: *action_ordinal,
                    },
                );
            };
            let Some(row) = quiescence
                .actions()
                .iter()
                .find(|row| row.action_ordinal() == *action_ordinal)
            else {
                complete = false;
                continue;
            };
            debug_assert_eq!(row.eligible_pending_keys(), expected_keys.as_slice());
            if row.result() != PrePublishUnstartedQuiescenceResult::QuiescedWithoutEntry {
                complete = false;
            }
            let staging_required = has_destination_staging_claim
                || action_has_required_role(&self.plan, *action_ordinal, WorkUnitRole::Stage);
            if staging_required
                && row.staging_absence() != Some(PrePublishAbsenceObservation::Absent)
            {
                complete = false;
            }
            let recovery_required = has_operation_recovery_claim
                || action_has_required_role(
                    &self.plan,
                    *action_ordinal,
                    WorkUnitRole::RecoveryMaterialRetained,
                );
            if recovery_required
                && row.operation_recovery_payload_absence()
                    != Some(PrePublishAbsenceObservation::Absent)
            {
                complete = false;
            }
        }
        Ok(complete)
    }

    fn validate_pre_publish_settlement(
        &self,
        inventory: &CancellationInventory,
        admission_claims: &AdmissionClaimInventory,
        cleanup: &PrePublishCleanupObservation,
        settlement: &AdmissionClaimSettlementInventory,
        unstarted_quiescence_complete: bool,
        dispatch_complete: bool,
    ) -> Result<(), WorkProgressError> {
        settlement
            .validate_against(admission_claims)
            .map_err(WorkProgressError::PrePublishCancellationSettlement)?;
        for claim in admission_claims.claims() {
            let observed = settlement
                .settlements()
                .iter()
                .find(|row| row.claim_id() == claim.claim_id())
                .expect("validated settlement coverage");
            if observed.outcome() == AdmissionClaimSettlementOutcome::Held {
                continue;
            }
            if !dispatch_complete {
                return Err(WorkProgressError::PrePublishCancellationDispatchIncomplete);
            }
            if !unstarted_quiescence_complete {
                return Err(WorkProgressError::PrePublishCancellationSettlementGateIncomplete);
            }
            let compatible = match claim.allocation_class() {
                FullCapacityAllocationClass::DestinationStaging
                | FullCapacityAllocationClass::OperationRecoveryPayload => {
                    observed.outcome() == AdmissionClaimSettlementOutcome::Released
                }
                FullCapacityAllocationClass::JournalRecord
                | FullCapacityAllocationClass::ExistingDestination
                | FullCapacityAllocationClass::ExistingRecoveryPayload => {
                    observed.outcome() == AdmissionClaimSettlementOutcome::ConsumedOrTransferred
                }
                FullCapacityAllocationClass::SourceDatabaseCommit
                | FullCapacityAllocationClass::SourceWalShm
                | FullCapacityAllocationClass::GlobalDatabaseCommit
                | FullCapacityAllocationClass::GlobalWalShm => matches!(
                    observed.outcome(),
                    AdmissionClaimSettlementOutcome::Released
                        | AdmissionClaimSettlementOutcome::ConsumedOrTransferred
                ),
            };
            if !compatible {
                return Err(
                    WorkProgressError::PrePublishCancellationSettlementClassMismatch {
                        claim_id: claim.claim_id(),
                        allocation_class: claim.allocation_class(),
                        outcome: observed.outcome(),
                    },
                );
            }
            if observed.outcome() == AdmissionClaimSettlementOutcome::Released {
                let absence_role = match claim.allocation_class() {
                    FullCapacityAllocationClass::DestinationStaging => Some(WorkUnitRole::Stage),
                    FullCapacityAllocationClass::OperationRecoveryPayload => {
                        Some(WorkUnitRole::RecoveryMaterialRetained)
                    }
                    _ => None,
                };
                if let Some(role) = absence_role {
                    let ready = self.pre_publish_absence_ready(
                        inventory,
                        cleanup,
                        role,
                        unstarted_quiescence_complete,
                    );
                    if !ready {
                        return Err(
                            WorkProgressError::PrePublishCancellationSettlementCleanupIncomplete {
                                allocation_class: claim.allocation_class(),
                            },
                        );
                    }
                }
            }
        }
        Ok(())
    }

    fn pre_publish_absence_ready(
        &self,
        inventory: &CancellationInventory,
        cleanup: &PrePublishCleanupObservation,
        role: WorkUnitRole,
        unstarted_quiescence_complete: bool,
    ) -> bool {
        if !unstarted_quiescence_complete {
            return false;
        }
        let has_operation_recovery_claim = self.admission_claims.as_ref().is_some_and(|claims| {
            claims.claims().iter().any(|claim| {
                claim.allocation_class() == FullCapacityAllocationClass::OperationRecoveryPayload
            })
        });
        inventory.actions().iter().all(|action| {
            if action.state() != CancellationActionState::CleanablePrePublish {
                return true;
            }
            let required =
                if role == WorkUnitRole::RecoveryMaterialRetained && has_operation_recovery_claim {
                    true
                } else {
                    action_has_required_role(&self.plan, action.action_ordinal(), role)
                };
            if !required {
                return true;
            }
            let Some(row) = cleanup
                .actions()
                .iter()
                .find(|row| row.action_ordinal() == action.action_ordinal())
            else {
                return false;
            };
            let observation = if role == WorkUnitRole::Stage {
                row.staging_absence()
            } else {
                row.operation_recovery_payload_absence()
            };
            observation == Some(PrePublishAbsenceObservation::Absent)
        })
    }

    /// Cancellation outcomes remain unavailable until recovery and settlement authorities are
    /// implemented.  The placeholder authority cannot advance progress.
    #[allow(dead_code)]
    fn finish_post_publish_cancellation(
        &mut self,
        _authority: PostPublishCancellationAuthority,
    ) -> Result<(), WorkProgressError> {
        Err(WorkProgressError::CancellationOutcomeUnavailable)
    }

    fn derive_cancellation_coordinator_closure_keys(
        &self,
        recovery_inventory: &CancellationEffectRecoveryInventory,
        dispatch_outcome: &CancellationDispatchOutcomeRecord,
    ) -> Vec<WorkUnitKey> {
        let mut has_recovery_effect = !recovery_inventory.bundles().is_empty();
        let mut affected_source_fences = Vec::<StoredSourceFence>::new();
        let mut retain_source_identity = |context: &CompletionContext| {
            let Some(fence) = context.scope().source_fence() else {
                return;
            };
            if !affected_source_fences
                .iter()
                .any(|candidate| same_stable_source_identity(candidate, fence))
            {
                affected_source_fences.push(fence.clone());
            }
        };

        for bundle in recovery_inventory.bundles() {
            for closure in bundle.source_closure() {
                if closure.status() == ProgressStepStatus::NotApplicable {
                    continue;
                }
                if let Some(scope) = self.scope_for(closure.key()) {
                    if scope.source_fence().is_some() {
                        let context = CompletionContext::new(self.execution_epoch, scope.clone());
                        retain_source_identity(&context);
                    }
                }
            }
        }
        for step in &self.steps {
            if step.status != ProgressStepStatus::Completed
                || !matches!(
                    step.evidence.as_ref(),
                    Some(StoredStepEvidence::SourceCommitted { .. })
                )
            {
                continue;
            }
            has_recovery_effect = true;
            if let Some(context) = step.completion_context.as_ref() {
                retain_source_identity(context);
            }
        }
        for row in dispatch_outcome.rows() {
            let CancellationDispatchOutcome::ObservedAfterCancellation { evidence, context } =
                row.outcome()
            else {
                continue;
            };
            if !matches!(evidence, StoredStepEvidence::SourceCommitted { .. }) {
                continue;
            }
            has_recovery_effect = true;
            retain_source_identity(context);
        }

        if !has_recovery_effect {
            return Vec::new();
        }
        let mut keys = self
            .plan
            .units()
            .iter()
            .filter(|unit| {
                unit.key().action_ordinal() == GLOBAL_ACTION_ORDINAL
                    && unit.requirement() == ParticipantRequirement::Required
                    && unit.applicability() == WorkUnitApplicability::Required
                    && matches!(
                        unit.key().role(),
                        WorkUnitRole::GlobalLibrary
                            | WorkUnitRole::Harvest
                            | WorkUnitRole::Projection
                            | WorkUnitRole::Readiness
                            | WorkUnitRole::Artifact
                    )
            })
            .filter(|unit| {
                let Some(scope) = self.scope_for(unit.key()) else {
                    return false;
                };
                match scope.source_fence() {
                    Some(fence) => affected_source_fences
                        .iter()
                        .any(|candidate| same_stable_source_identity(candidate, fence)),
                    None => true,
                }
            })
            .map(|unit| unit.key())
            .collect::<Vec<_>>();
        keys.sort();
        keys.dedup();
        keys
    }

    fn cancellation_source_revision_floor(
        &self,
        fence: &StoredSourceFence,
        dispatch_outcome: &CancellationDispatchOutcomeRecord,
        source_closure_observation: Option<&CancellationSourceClosureObservationRecord>,
    ) -> Option<u64> {
        let mut minimum = None;
        let mut consider = |context: &CompletionContext, evidence: &StoredStepEvidence| {
            let Some(context_fence) = context.scope().source_fence() else {
                return;
            };
            if !same_stable_source_identity(context_fence, fence) {
                return;
            }
            let Some(observation) = source_observation_for_evidence(evidence) else {
                return;
            };
            if !same_stable_source_identity_observation(observation, fence) {
                return;
            }
            minimum = Some(minimum.unwrap_or(0).max(observation.revision()));
        };

        for step in &self.steps {
            if step.status != ProgressStepStatus::Completed {
                continue;
            }
            if let (Some(context), Some(evidence)) =
                (step.completion_context.as_ref(), step.evidence.as_ref())
            {
                consider(context, evidence);
            }
        }
        for row in dispatch_outcome.rows() {
            let CancellationDispatchOutcome::ObservedAfterCancellation { evidence, context } =
                row.outcome()
            else {
                continue;
            };
            consider(context, evidence);
        }
        if let Some(source_closure_observation) = source_closure_observation {
            for row in source_closure_observation.rows() {
                let CancellationSourceClosureOutcome::Observed { evidence, context } =
                    row.outcome()
                else {
                    continue;
                };
                consider(context, evidence);
            }
        }
        for batch in &self.readiness_deferral_batches {
            for row in batch.rows() {
                let evidence = StoredStepEvidence::ReadinessScheduled {
                    observation: row.clone(),
                };
                consider(row.context(), &evidence);
            }
        }
        minimum
    }

    #[allow(dead_code)]
    fn scope_for(&self, key: WorkUnitKey) -> Option<&ParticipantScope> {
        self.scopes
            .iter()
            .find(|scope| scope.participant_id == key.participant_id())
    }

    fn minimum_source_revision_for_fence(&self, fence: &StoredSourceFence) -> Option<u64> {
        let mut minimum = self
            .steps
            .iter()
            .filter(|step| {
                step.completion_context
                    .as_ref()
                    .and_then(|context| context.scope.source_fence())
                    .is_some_and(|candidate| candidate == fence)
            })
            .filter_map(|step| {
                step.evidence
                    .as_ref()
                    .and_then(source_observation_for_evidence)
                    .map(SourceRevisionObservation::revision)
            })
            .max();
        if let Some(record) = self
            .pre_publish_cancellation
            .as_ref()
            .and_then(PrePublishCancellationObservation::dispatch_outcome)
        {
            for row in record.rows() {
                let CancellationDispatchOutcome::ObservedAfterCancellation { evidence, context } =
                    row.outcome()
                else {
                    continue;
                };
                if context
                    .scope()
                    .source_fence()
                    .is_some_and(|candidate| candidate == fence)
                {
                    if let Some(observation) = source_observation_for_evidence(evidence) {
                        minimum = Some(minimum.unwrap_or(0).max(observation.revision()));
                    }
                }
            }
        }
        if let Some(accepted) = self.after_effect_reconciliation.as_ref() {
            for row in accepted.recovery_inventory().dispatch_outcome().rows() {
                let CancellationDispatchOutcome::ObservedAfterCancellation { evidence, context } =
                    row.outcome()
                else {
                    continue;
                };
                if context
                    .scope()
                    .source_fence()
                    .is_some_and(|candidate| candidate == fence)
                {
                    if let Some(observation) = source_observation_for_evidence(evidence) {
                        minimum = Some(minimum.unwrap_or(0).max(observation.revision()));
                    }
                }
            }
            for row in accepted.source_closure_observation().rows() {
                let CancellationSourceClosureOutcome::Observed { evidence, context } =
                    row.outcome()
                else {
                    continue;
                };
                if context
                    .scope()
                    .source_fence()
                    .is_some_and(|candidate| candidate == fence)
                {
                    if let Some(observation) = source_observation_for_evidence(evidence) {
                        minimum = Some(minimum.unwrap_or(0).max(observation.revision()));
                    }
                }
            }
            for row in accepted.coordinator_closure_observation().rows() {
                let CancellationCoordinatorClosureOutcome::Observed { evidence, context } =
                    row.outcome()
                else {
                    continue;
                };
                if context
                    .scope()
                    .source_fence()
                    .is_some_and(|candidate| candidate == fence)
                {
                    if let Some(observation) = source_observation_for_evidence(evidence) {
                        minimum = Some(minimum.unwrap_or(0).max(observation.revision()));
                    }
                }
            }
        }
        for batch in &self.readiness_deferral_batches {
            for row in batch.rows() {
                if row
                    .context()
                    .scope()
                    .source_fence()
                    .is_some_and(|candidate| candidate == fence)
                {
                    minimum = Some(
                        minimum
                            .unwrap_or(0)
                            .max(row.source_observation().revision()),
                    );
                }
            }
        }
        minimum
    }

    fn minimum_source_revision_for_stable_source(&self, fence: &StoredSourceFence) -> u64 {
        let mut minimum = fence.source_revision();
        for scope in &self.scopes {
            if let Some(scope_fence) = scope.source_fence.as_ref() {
                if same_stable_source_identity(scope_fence, fence) {
                    minimum = minimum.max(scope_fence.source_revision());
                }
            }
        }
        for step in &self.steps {
            let Some(context) = step.completion_context.as_ref() else {
                continue;
            };
            let Some(scope_fence) = context.scope.source_fence() else {
                continue;
            };
            if !same_stable_source_identity(scope_fence, fence) {
                continue;
            }
            let observation = step
                .evidence
                .as_ref()
                .and_then(source_observation_for_evidence);
            if let Some(observation) = observation {
                if same_stable_source_identity_observation(observation, fence) {
                    minimum = minimum.max(observation.revision());
                }
            }
        }
        if let Some(record) = self
            .pre_publish_cancellation
            .as_ref()
            .and_then(PrePublishCancellationObservation::dispatch_outcome)
        {
            for row in record.rows() {
                let CancellationDispatchOutcome::ObservedAfterCancellation { evidence, context } =
                    row.outcome()
                else {
                    continue;
                };
                let Some(context_fence) = context.scope().source_fence() else {
                    continue;
                };
                if !same_stable_source_identity(context_fence, fence) {
                    continue;
                }
                if let Some(observation) = source_observation_for_evidence(evidence) {
                    if same_stable_source_identity_observation(observation, fence) {
                        minimum = minimum.max(observation.revision());
                    }
                }
            }
        }
        if let Some(accepted) = self.after_effect_reconciliation.as_ref() {
            for row in accepted.recovery_inventory().dispatch_outcome().rows() {
                let CancellationDispatchOutcome::ObservedAfterCancellation { evidence, context } =
                    row.outcome()
                else {
                    continue;
                };
                let Some(context_fence) = context.scope().source_fence() else {
                    continue;
                };
                if !same_stable_source_identity(context_fence, fence) {
                    continue;
                }
                if let Some(observation) = source_observation_for_evidence(evidence) {
                    if same_stable_source_identity_observation(observation, fence) {
                        minimum = minimum.max(observation.revision());
                    }
                }
            }
            for row in accepted.source_closure_observation().rows() {
                let CancellationSourceClosureOutcome::Observed { evidence, context } =
                    row.outcome()
                else {
                    continue;
                };
                let Some(context_fence) = context.scope().source_fence() else {
                    continue;
                };
                if !same_stable_source_identity(context_fence, fence) {
                    continue;
                }
                if let Some(observation) = source_observation_for_evidence(evidence) {
                    if same_stable_source_identity_observation(observation, fence) {
                        minimum = minimum.max(observation.revision());
                    }
                }
            }
            for row in accepted.coordinator_closure_observation().rows() {
                let CancellationCoordinatorClosureOutcome::Observed { evidence, context } =
                    row.outcome()
                else {
                    continue;
                };
                let Some(context_fence) = context.scope().source_fence() else {
                    continue;
                };
                if !same_stable_source_identity(context_fence, fence) {
                    continue;
                }
                if let Some(observation) = source_observation_for_evidence(evidence) {
                    if same_stable_source_identity_observation(observation, fence) {
                        minimum = minimum.max(observation.revision());
                    }
                }
            }
        }
        for batch in &self.readiness_deferral_batches {
            for row in batch.rows() {
                let Some(context_fence) = row.context().scope().source_fence() else {
                    continue;
                };
                if same_stable_source_identity(context_fence, fence)
                    && same_stable_source_identity_observation(row.source_observation(), fence)
                {
                    minimum = minimum.max(row.source_observation().revision());
                }
            }
        }
        minimum
    }

    fn refresh_derived_state(&mut self) {
        if self.disposition.is_terminal()
            || matches!(
                self.disposition,
                FullProgressDisposition::AuditRequired
                    | FullProgressDisposition::CancelRequestedBeforePublish
                    | FullProgressDisposition::CancelRequestedAfterPublish
                    | FullProgressDisposition::CancelRequestedAfterObservedEffect
            )
        {
            return;
        }
        let candidate = self.derived_phase();
        if phase_rank(candidate) > phase_rank(self.phase) {
            self.phase = candidate;
        }
        // A terminal success also needs a private claim-settlement authority.  Keeping this
        // derived refresh nonterminal prevents an unadmitted or unsettled observation from being
        // mistaken for durable success.
    }

    fn derived_phase(&self) -> OperationPhase {
        let has_publication_boundary = self.steps.iter().any(|step| {
            matches!(
                step.key.role(),
                WorkUnitRole::Publish | WorkUnitRole::QualifiedRelocation
            ) && step.status != ProgressStepStatus::NotApplicable
        });
        let publication_seen = self.steps.iter().any(|step| {
            matches!(
                step.key.role(),
                WorkUnitRole::Publish | WorkUnitRole::QualifiedRelocation
            ) && step.status == ProgressStepStatus::Completed
        });
        let staging_seen = self.steps.iter().any(|step| {
            matches!(
                step.key.role(),
                WorkUnitRole::Stage | WorkUnitRole::CopyValidated
            ) && step.status == ProgressStepStatus::Completed
        });
        let mut phase = OperationPhase::IntentDurable;
        if self.all_required_role_complete(|role| role == WorkUnitRole::Prepare) {
            phase = OperationPhase::Prepared;
        }
        if staging_seen && !publication_seen {
            phase = OperationPhase::FilesystemStaged;
        }
        if publication_seen {
            phase = OperationPhase::FilesystemPublished;
        }
        let source_complete = self.all_required_role_complete(|role| {
            matches!(
                role,
                WorkUnitRole::SourceAffect
                    | WorkUnitRole::DestinationSourceReconcile
                    | WorkUnitRole::OriginSourceReconcile
            )
        });
        if source_complete
            && self.all_required_filesystem_complete()
            && (!has_publication_boundary || publication_seen)
        {
            phase = OperationPhase::SourceReconciled;
        }
        if phase_rank(phase) >= phase_rank(OperationPhase::SourceReconciled)
            && self.all_required_role_complete(|role| {
                matches!(role, WorkUnitRole::GlobalLibrary | WorkUnitRole::Harvest)
            })
        {
            phase = OperationPhase::GlobalReconciled;
        }
        if phase_rank(phase) >= phase_rank(OperationPhase::GlobalReconciled)
            && self.all_required_role_complete(|role| role == WorkUnitRole::Projection)
        {
            phase = OperationPhase::ProjectionPublished;
        }
        if phase_rank(phase) >= phase_rank(OperationPhase::ProjectionPublished)
            && self.all_required_role_complete(|role| role == WorkUnitRole::Readiness)
        {
            phase = OperationPhase::ReadinessScheduled;
        }
        phase
    }

    fn all_required_role_complete<F>(&self, predicate: F) -> bool
    where
        F: Fn(WorkUnitRole) -> bool,
    {
        let mut found = false;
        for step in &self.steps {
            if !predicate(step.key.role()) || step.requirement != ParticipantRequirement::Required {
                continue;
            }
            found = true;
            if step.status != ProgressStepStatus::Completed
                && step.status != ProgressStepStatus::NotApplicable
            {
                return false;
            }
        }
        found
    }

    fn all_required_filesystem_complete(&self) -> bool {
        self.steps.iter().all(|step| {
            expected_filesystem_receipt(step.key.role()).is_none()
                || step.requirement != ParticipantRequirement::Required
                || matches!(
                    step.status,
                    ProgressStepStatus::Completed | ProgressStepStatus::NotApplicable
                )
        })
    }

    fn all_required_complete(&self) -> bool {
        self.steps.iter().all(|step| {
            if step.requirement != ParticipantRequirement::Required {
                true
            } else {
                matches!(
                    step.status,
                    ProgressStepStatus::Completed | ProgressStepStatus::NotApplicable
                )
            }
        })
    }

    fn optional_artifacts_resolved(&self) -> bool {
        self.steps.iter().all(|step| {
            step.requirement != ParticipantRequirement::OptionalRebuildable
                || matches!(
                    step.status,
                    ProgressStepStatus::Completed | ProgressStepStatus::Deferred
                )
        })
    }

    fn has_deferred_optional_artifact(&self) -> bool {
        self.steps.iter().any(|step| {
            step.requirement == ParticipantRequirement::OptionalRebuildable
                && step.status == ProgressStepStatus::Deferred
        })
    }
}

fn derive_origin_policies(
    args: &UniversalCommandArgs,
    plan: &WorkPlan,
) -> Vec<PrePublishActionPolicy> {
    let create_folder = matches!(
        args,
        UniversalCommandArgs::Namespace(NamespaceCommandArgs::CreateFolder(_))
    );
    let mut policies = Vec::new();
    let mut previous = None;
    for unit in plan.units() {
        let action_ordinal = unit.key().action_ordinal();
        if previous == Some(action_ordinal) {
            continue;
        }
        policies.push(PrePublishActionPolicy {
            action_ordinal,
            origin: if create_folder && action_ordinal == 0 {
                PrePublishOriginPolicy::NoOriginalObject
            } else {
                PrePublishOriginPolicy::OriginalStatePreserved
            },
        });
        previous = Some(action_ordinal);
    }
    policies
}

fn action_has_required_role(plan: &WorkPlan, action_ordinal: u8, role: WorkUnitRole) -> bool {
    plan.units().iter().any(|unit| {
        unit.key().action_ordinal() == action_ordinal
            && unit.key().role() == role
            && unit.applicability() == WorkUnitApplicability::Required
    })
}

fn validate_pre_publish_settlement_transition(
    existing: &AdmissionClaimSettlementInventory,
    candidate: &AdmissionClaimSettlementInventory,
) -> Result<(), WorkProgressError> {
    for settled in existing.settlements() {
        let observed = candidate
            .settlements()
            .iter()
            .find(|row| row.claim_id() == settled.claim_id())
            .expect("validated settlement coverage");
        let allowed = match settled.outcome() {
            AdmissionClaimSettlementOutcome::Held => true,
            outcome => observed.outcome() == outcome,
        };
        if !allowed {
            return Err(WorkProgressError::PrePublishCancellationSettlementReplayConflict);
        }
    }
    Ok(())
}

fn validate_pre_publish_unstarted_quiescence_transition(
    existing: &PrePublishUnstartedQuiescenceObservation,
    candidate: &PrePublishUnstartedQuiescenceObservation,
) -> Result<(), WorkProgressError> {
    for existing_row in existing.actions() {
        let Some(candidate_row) = candidate
            .actions()
            .iter()
            .find(|row| row.action_ordinal() == existing_row.action_ordinal())
        else {
            if existing_row.result() == PrePublishUnstartedQuiescenceResult::QuiescedWithoutEntry
                || existing_row.staging_absence() == Some(PrePublishAbsenceObservation::Absent)
                || existing_row.operation_recovery_payload_absence()
                    == Some(PrePublishAbsenceObservation::Absent)
            {
                return Err(WorkProgressError::PrePublishCancellationQuiescenceReplayConflict);
            }
            continue;
        };
        let quiescence_regressed = existing_row.result()
            == PrePublishUnstartedQuiescenceResult::QuiescedWithoutEntry
            && candidate_row.result() != PrePublishUnstartedQuiescenceResult::QuiescedWithoutEntry;
        let staging_regressed = existing_row.staging_absence()
            == Some(PrePublishAbsenceObservation::Absent)
            && candidate_row.staging_absence() != Some(PrePublishAbsenceObservation::Absent);
        let recovery_regressed = existing_row.operation_recovery_payload_absence()
            == Some(PrePublishAbsenceObservation::Absent)
            && candidate_row.operation_recovery_payload_absence()
                != Some(PrePublishAbsenceObservation::Absent);
        if quiescence_regressed || staging_regressed || recovery_regressed {
            return Err(WorkProgressError::PrePublishCancellationQuiescenceReplayConflict);
        }
    }
    Ok(())
}

fn validate_readiness_deferral_source_revision_order(
    candidate: &ReadinessSchedulingObservationRow,
    normal: &ReadinessSchedulingObservationRow,
    previous_batches: &[ReadinessDeferralBatchObservation],
) -> Result<(), WorkProgressError> {
    let mut minimum = None;
    let candidate_observation = candidate.source_observation();
    if same_stable_source_observation_identity(candidate_observation, normal.source_observation()) {
        minimum = Some(normal.source_observation().revision());
    }
    for batch in previous_batches {
        for previous in batch.rows() {
            if same_stable_source_observation_identity(
                candidate_observation,
                previous.source_observation(),
            ) {
                minimum = Some(
                    minimum
                        .unwrap_or(0)
                        .max(previous.source_observation().revision()),
                );
            }
        }
    }
    if let Some(minimum) = minimum {
        if candidate_observation.revision() < minimum {
            return Err(
                WorkProgressError::ReadinessDeferralSourceRevisionRegression {
                    key: candidate.readiness_key(),
                    observed: candidate_observation.revision(),
                    minimum,
                },
            );
        }
    }
    Ok(())
}

fn validate_readiness_publication_order(
    normal: Option<&ReadinessSchedulingObservationRow>,
    previous: Option<&ReadinessSchedulingObservationRow>,
    candidate: &ReadinessSchedulingObservationRow,
) -> Result<(), WorkProgressError> {
    for reference in [normal, previous].into_iter().flatten() {
        let source_unchanged = candidate.source_generation() == reference.source_generation();
        let revision_unchanged = candidate.readiness_revision() == reference.readiness_revision();
        if source_unchanged && revision_unchanged {
            if candidate.contract_version() != reference.contract_version()
                || candidate.target_set() != reference.target_set()
                || candidate.wake() != reference.wake()
            {
                return Err(WorkProgressError::ReadinessDeferralPublicationConflict {
                    key: candidate.readiness_key(),
                });
            }
            continue;
        }
        if candidate.source_generation() < reference.source_generation() {
            return Err(
                WorkProgressError::ReadinessDeferralSourceGenerationRegression {
                    key: candidate.readiness_key(),
                },
            );
        }
        if candidate.readiness_revision() <= reference.readiness_revision() {
            return Err(WorkProgressError::ReadinessDeferralRevisionRegression {
                key: candidate.readiness_key(),
            });
        }
    }
    Ok(())
}

fn validate_after_effect_phase_gates(
    dispatch_outcome: &CancellationDispatchOutcomeRecord,
    source_closure_observation: &CancellationSourceClosureObservationRecord,
    coordinator_closure_observation: &CancellationCoordinatorClosureObservationRecord,
) -> Result<(), WorkProgressError> {
    let dispatch_determinate = dispatch_outcome
        .rows()
        .iter()
        .all(|row| !matches!(row.outcome(), CancellationDispatchOutcome::EntryUncertain));
    let source_observed = source_closure_observation.rows().iter().any(|row| {
        matches!(
            row.outcome(),
            CancellationSourceClosureOutcome::Observed { .. }
        )
    });
    let coordinator_observed = coordinator_closure_observation.rows().iter().any(|row| {
        matches!(
            row.outcome(),
            CancellationCoordinatorClosureOutcome::Observed { .. }
        )
    });
    if !dispatch_determinate && (source_observed || coordinator_observed) {
        return Err(WorkProgressError::AfterEffectReconciliationDispatchIndeterminate);
    }
    if coordinator_observed
        && source_closure_observation
            .rows()
            .iter()
            .any(|row| matches!(row.outcome(), CancellationSourceClosureOutcome::Pending))
    {
        return Err(WorkProgressError::AfterEffectReconciliationSourceClosureIncomplete);
    }
    Ok(())
}

fn cancellation_dispatch_effects_equal(
    left: &CancellationDispatchOutcomeRecord,
    right: &CancellationDispatchOutcomeRecord,
) -> bool {
    left.manifest() == right.manifest()
        && left.rows().len() == right.rows().len()
        && left
            .rows()
            .iter()
            .zip(right.rows())
            .all(|(left, right)| left.key() == right.key() && left.outcome() == right.outcome())
}

fn resource_cleanup_observed_revision_and_context(
    row: &CancellationResourceCleanupObservationRow,
) -> Option<(u64, &CompletionContext)> {
    match row.outcome() {
        CancellationResourceCleanupOutcome::Pending => None,
        CancellationResourceCleanupOutcome::Preserved {
            owner_revision,
            context,
        }
        | CancellationResourceCleanupOutcome::Absent {
            owner_revision,
            context,
        } => Some((*owner_revision, &context)),
    }
}

fn namespace_cleanup_observed_revision_and_context(
    row: &CancellationNamespaceCleanupObservationRow,
) -> Option<(u64, &CompletionContext)> {
    match row.outcome() {
        CancellationNamespaceCleanupOutcome::Pending => None,
        CancellationNamespaceCleanupOutcome::NoOperationClaim {
            owner_revision,
            context,
        }
        | CancellationNamespaceCleanupOutcome::Preserved {
            owner_revision,
            context,
        }
        | CancellationNamespaceCleanupOutcome::Absent {
            owner_revision,
            context,
        } => Some((*owner_revision, &context)),
    }
}

fn validate_resource_cleanup_transition(
    previous: &CancellationResourceCleanupObservationRecord,
    candidate: &CancellationResourceCleanupObservationRecord,
    current_execution_epoch: u64,
    dispatch_changed: bool,
) -> Result<(), WorkProgressError> {
    if previous != candidate
        && candidate.observation_generation() <= previous.observation_generation()
    {
        return Err(
            WorkProgressError::CancellationCleanupObservationGenerationRegression {
                previous: previous.observation_generation(),
                actual: candidate.observation_generation(),
            },
        );
    }
    for (previous_row, candidate_row) in previous.rows().iter().zip(candidate.rows()) {
        let Some((previous_revision, _)) =
            resource_cleanup_observed_revision_and_context(previous_row)
        else {
            if let Some((_, candidate_context)) =
                resource_cleanup_observed_revision_and_context(candidate_row)
            {
                if candidate_context.execution_epoch() != current_execution_epoch {
                    return Err(
                        WorkProgressError::CancellationCleanupObservationEpochMismatch {
                            key: candidate_row.key(),
                            expected: current_execution_epoch,
                            actual: candidate_context.execution_epoch(),
                        },
                    );
                }
            }
            continue;
        };
        let Some((candidate_revision, candidate_context)) =
            resource_cleanup_observed_revision_and_context(candidate_row)
        else {
            return Err(
                WorkProgressError::CancellationCleanupObservationReplayConflict {
                    key: previous_row.key(),
                },
            );
        };
        let changed = previous_row.outcome() != candidate_row.outcome();
        if changed || dispatch_changed {
            if candidate_revision <= previous_revision {
                return Err(
                    WorkProgressError::CancellationCleanupOwnerRevisionRegression {
                        key: candidate_row.key(),
                        previous: previous_revision,
                        actual: candidate_revision,
                    },
                );
            }
            if candidate_context.execution_epoch() != current_execution_epoch {
                return Err(
                    WorkProgressError::CancellationCleanupObservationEpochMismatch {
                        key: candidate_row.key(),
                        expected: current_execution_epoch,
                        actual: candidate_context.execution_epoch(),
                    },
                );
            }
        }
    }
    Ok(())
}

fn validate_resource_cleanup_initial_contexts(
    candidate: &CancellationResourceCleanupObservationRecord,
    current_execution_epoch: u64,
) -> Result<(), WorkProgressError> {
    for row in candidate.rows() {
        if let Some((_, context)) = resource_cleanup_observed_revision_and_context(row) {
            if context.execution_epoch() != current_execution_epoch {
                return Err(
                    WorkProgressError::CancellationCleanupObservationEpochMismatch {
                        key: row.key(),
                        expected: current_execution_epoch,
                        actual: context.execution_epoch(),
                    },
                );
            }
        }
    }
    Ok(())
}

fn validate_namespace_cleanup_transition(
    previous: &CancellationNamespaceCleanupObservationRecord,
    candidate: &CancellationNamespaceCleanupObservationRecord,
    current_execution_epoch: u64,
    dispatch_changed: bool,
) -> Result<(), WorkProgressError> {
    if previous != candidate
        && candidate.observation_generation() <= previous.observation_generation()
    {
        return Err(
            WorkProgressError::CancellationCleanupObservationGenerationRegression {
                previous: previous.observation_generation(),
                actual: candidate.observation_generation(),
            },
        );
    }
    for (previous_row, candidate_row) in previous.rows().iter().zip(candidate.rows()) {
        let Some((previous_revision, _)) =
            namespace_cleanup_observed_revision_and_context(previous_row)
        else {
            if let Some((_, candidate_context)) =
                namespace_cleanup_observed_revision_and_context(candidate_row)
            {
                if candidate_context.execution_epoch() != current_execution_epoch {
                    return Err(
                        WorkProgressError::CancellationCleanupObservationEpochMismatch {
                            key: candidate_row.key(),
                            expected: current_execution_epoch,
                            actual: candidate_context.execution_epoch(),
                        },
                    );
                }
            }
            continue;
        };
        let Some((candidate_revision, candidate_context)) =
            namespace_cleanup_observed_revision_and_context(candidate_row)
        else {
            return Err(
                WorkProgressError::CancellationCleanupObservationReplayConflict {
                    key: previous_row.key(),
                },
            );
        };
        let changed = previous_row.outcome() != candidate_row.outcome();
        if changed || dispatch_changed {
            if candidate_revision <= previous_revision {
                return Err(
                    WorkProgressError::CancellationCleanupOwnerRevisionRegression {
                        key: candidate_row.key(),
                        previous: previous_revision,
                        actual: candidate_revision,
                    },
                );
            }
            if candidate_context.execution_epoch() != current_execution_epoch {
                return Err(
                    WorkProgressError::CancellationCleanupObservationEpochMismatch {
                        key: candidate_row.key(),
                        expected: current_execution_epoch,
                        actual: candidate_context.execution_epoch(),
                    },
                );
            }
        }
    }
    Ok(())
}

fn validate_namespace_cleanup_initial_contexts(
    candidate: &CancellationNamespaceCleanupObservationRecord,
    current_execution_epoch: u64,
) -> Result<(), WorkProgressError> {
    for row in candidate.rows() {
        if let Some((_, context)) = namespace_cleanup_observed_revision_and_context(row) {
            if context.execution_epoch() != current_execution_epoch {
                return Err(
                    WorkProgressError::CancellationCleanupObservationEpochMismatch {
                        key: row.key(),
                        expected: current_execution_epoch,
                        actual: context.execution_epoch(),
                    },
                );
            }
        }
    }
    Ok(())
}

fn validate_after_effect_source_transition(
    previous: &CancellationSourceClosureObservationRecord,
    candidate: &CancellationSourceClosureObservationRecord,
) -> Result<(), WorkProgressError> {
    if candidate.observation_generation() <= previous.observation_generation() {
        return Err(WorkProgressError::AfterEffectReconciliationReceiptReplayConflict);
    }
    let previous_has_observed = previous.rows().iter().any(|row| {
        matches!(
            row.outcome(),
            CancellationSourceClosureOutcome::Observed { .. }
        )
    });
    if previous_has_observed && previous.manifest().keys() != candidate.manifest().keys() {
        return Err(WorkProgressError::AfterEffectReconciliationReceiptReplayConflict);
    }
    for previous_row in previous.rows() {
        let Some(candidate_row) = candidate
            .rows()
            .iter()
            .find(|row| row.key() == previous_row.key())
        else {
            if matches!(
                previous_row.outcome(),
                CancellationSourceClosureOutcome::Observed { .. }
            ) {
                return Err(WorkProgressError::AfterEffectReconciliationReceiptReplayConflict);
            }
            continue;
        };
        if let CancellationSourceClosureOutcome::Observed { .. } = previous_row.outcome()
            && candidate_row.outcome() != previous_row.outcome()
        {
            return Err(WorkProgressError::AfterEffectReconciliationReceiptReplayConflict);
        }
    }
    Ok(())
}

fn validate_after_effect_coordinator_transition(
    previous: &CancellationCoordinatorClosureObservationRecord,
    candidate: &CancellationCoordinatorClosureObservationRecord,
) -> Result<(), WorkProgressError> {
    if candidate.observation_generation() <= previous.observation_generation() {
        return Err(WorkProgressError::AfterEffectReconciliationReceiptReplayConflict);
    }
    let previous_has_observed = previous.rows().iter().any(|row| {
        matches!(
            row.outcome(),
            CancellationCoordinatorClosureOutcome::Observed { .. }
        )
    });
    if previous_has_observed
        && (previous.manifest().keys() != candidate.manifest().keys()
            || previous
                .manifest()
                .source_closure_observation()
                .manifest()
                .keys()
                != candidate
                    .manifest()
                    .source_closure_observation()
                    .manifest()
                    .keys())
    {
        return Err(WorkProgressError::AfterEffectReconciliationReceiptReplayConflict);
    }
    for previous_row in previous.rows() {
        let Some(candidate_row) = candidate
            .rows()
            .iter()
            .find(|row| row.key() == previous_row.key())
        else {
            if matches!(
                previous_row.outcome(),
                CancellationCoordinatorClosureOutcome::Observed { .. }
            ) {
                return Err(WorkProgressError::AfterEffectReconciliationReceiptReplayConflict);
            }
            continue;
        };
        if let CancellationCoordinatorClosureOutcome::Observed { .. } = previous_row.outcome()
            && candidate_row.outcome() != previous_row.outcome()
        {
            return Err(WorkProgressError::AfterEffectReconciliationReceiptReplayConflict);
        }
    }
    Ok(())
}

#[allow(dead_code)]
fn phase_rank(phase: OperationPhase) -> u8 {
    match phase {
        OperationPhase::IntentDurable => 0,
        OperationPhase::Prepared => 1,
        OperationPhase::FilesystemStaged => 2,
        OperationPhase::FilesystemPublished => 3,
        OperationPhase::SourceReconciled => 4,
        OperationPhase::GlobalReconciled => 5,
        OperationPhase::ProjectionPublished => 6,
        OperationPhase::ReadinessScheduled => 7,
        OperationPhase::Terminal => 8,
    }
}

#[allow(dead_code)]
fn expected_filesystem_receipt(role: WorkUnitRole) -> Option<FilesystemReceiptKind> {
    Some(match role {
        WorkUnitRole::Prepare => FilesystemReceiptKind::Prepared,
        WorkUnitRole::RecoveryMaterialRetained => FilesystemReceiptKind::RecoveryMaterialRetained,
        WorkUnitRole::RestoreRecordPrepared => FilesystemReceiptKind::RestoreRecordPrepared,
        WorkUnitRole::EnsureContainedParentDirectories => {
            FilesystemReceiptKind::EnsureContainedParentDirectories
        }
        WorkUnitRole::Stage => FilesystemReceiptKind::Stage,
        WorkUnitRole::CopyValidated => FilesystemReceiptKind::CopyValidated,
        WorkUnitRole::Publish => FilesystemReceiptKind::Publish,
        WorkUnitRole::QualifiedRelocation => FilesystemReceiptKind::QualifiedRelocation,
        WorkUnitRole::RetainedPayloadVerified => FilesystemReceiptKind::RetainedPayloadVerified,
        WorkUnitRole::SelectedRetainedObjectDisposition => {
            FilesystemReceiptKind::SelectedRetainedObjectDisposition
        }
        WorkUnitRole::RetainedObjectAbsence => FilesystemReceiptKind::RetainedObjectAbsence,
        WorkUnitRole::OriginRemovalStart => FilesystemReceiptKind::OriginRemovalStart,
        WorkUnitRole::OriginAbsence => FilesystemReceiptKind::OriginAbsence,
        WorkUnitRole::RetainedMerge => FilesystemReceiptKind::RetainedMerge,
        WorkUnitRole::ExternalAudit => FilesystemReceiptKind::ExternalAudit,
        _ => return None,
    })
}

#[allow(dead_code)]
fn expected_coordinator_receipt(role: WorkUnitRole) -> Option<CoordinatorReceiptKind> {
    Some(match role {
        WorkUnitRole::GlobalLibrary => CoordinatorReceiptKind::GlobalLibrary,
        WorkUnitRole::Harvest => CoordinatorReceiptKind::Harvest,
        WorkUnitRole::Projection => CoordinatorReceiptKind::Projection,
        WorkUnitRole::Readiness => CoordinatorReceiptKind::Readiness,
        WorkUnitRole::Artifact => CoordinatorReceiptKind::Artifact,
        _ => return None,
    })
}

#[allow(dead_code)]
fn validate_evidence_for_role(
    role: WorkUnitRole,
    scope: &ParticipantScope,
    evidence: &StoredStepEvidence,
    minimum_revision: Option<u64>,
) -> Result<(), WorkProgressError> {
    if let Some(expected) = expected_filesystem_receipt(role) {
        if scope.kind != ParticipantKind::Filesystem {
            return Err(WorkProgressError::ParticipantKindMismatch {
                role,
                expected: ParticipantKind::Filesystem,
                actual: scope.kind,
            });
        }
        let StoredStepEvidence::Filesystem { receipt } = evidence else {
            return Err(WorkProgressError::RoleEvidenceMismatch { role });
        };
        if receipt.kind() != expected {
            return Err(WorkProgressError::RoleEvidenceMismatch { role });
        }
        return Ok(());
    }
    if let Some(expected) = expected_coordinator_receipt(role) {
        let expected_kind = match role {
            WorkUnitRole::GlobalLibrary
            | WorkUnitRole::Harvest
            | WorkUnitRole::Projection
            | WorkUnitRole::Readiness
            | WorkUnitRole::Artifact => match role {
                WorkUnitRole::GlobalLibrary => ParticipantKind::GlobalLibrary,
                WorkUnitRole::Harvest => ParticipantKind::Harvest,
                WorkUnitRole::Projection => ParticipantKind::Projection,
                WorkUnitRole::Readiness => ParticipantKind::Readiness,
                WorkUnitRole::Artifact => ParticipantKind::Artifact,
                _ => unreachable!(),
            },
            _ => unreachable!(),
        };
        if scope.kind != expected_kind {
            return Err(WorkProgressError::ParticipantKindMismatch {
                role,
                expected: expected_kind,
                actual: scope.kind,
            });
        }
        if role == WorkUnitRole::Readiness {
            if let StoredStepEvidence::ReadinessScheduled { observation } = evidence {
                observation
                    .validate_for_stored_step_evidence()
                    .map_err(WorkProgressError::ReadinessSchedulingObservation)?;
                let fence = scope
                    .source_fence
                    .as_ref()
                    .ok_or(WorkProgressError::SourceFenceMissing { role })?;
                validate_source_observation(
                    role,
                    observation.source_observation(),
                    fence,
                    minimum_revision,
                )?;
                return Ok(());
            }
        }
        let StoredStepEvidence::Coordinator { receipt } = evidence else {
            return Err(WorkProgressError::RoleEvidenceMismatch { role });
        };
        if receipt.kind() != expected {
            return Err(WorkProgressError::RoleEvidenceMismatch { role });
        }
        match (scope.source_fence(), receipt.source_observation()) {
            (Some(fence), Some(observation)) => {
                validate_source_observation(role, observation, fence, minimum_revision)?
            }
            (Some(_), None) => {
                return Err(WorkProgressError::CoordinatorSourceObservationMissing { role });
            }
            (None, Some(_)) => {
                return Err(WorkProgressError::CoordinatorSourceObservationForbidden { role });
            }
            (None, None) => {}
        }
        if receipt.deficit().is_some() {
            return Err(WorkProgressError::CoordinatorDeficitPresent { role });
        }
        return Ok(());
    }

    let expected_kind = ParticipantKind::SourceDatabase;
    if scope.kind != expected_kind || scope.source_fence.is_none() {
        return Err(WorkProgressError::ParticipantKindMismatch {
            role,
            expected: expected_kind,
            actual: scope.kind,
        });
    }
    let (observation, before_revision) = match (role, evidence) {
        (WorkUnitRole::SourceRead, StoredStepEvidence::SourceObserved { observation }) => {
            (observation, None)
        }
        (
            WorkUnitRole::SourceAffect
            | WorkUnitRole::DestinationSourceReconcile
            | WorkUnitRole::OriginSourceReconcile,
            StoredStepEvidence::SourceCommitted {
                before_revision,
                after,
            },
        ) => (after, Some(*before_revision)),
        _ => return Err(WorkProgressError::RoleEvidenceMismatch { role }),
    };
    let fence = scope
        .source_fence
        .as_ref()
        .ok_or(WorkProgressError::SourceFenceMissing { role })?;
    validate_source_observation(role, observation, fence, minimum_revision)?;
    if let Some(before_revision) = before_revision {
        let floor = fence
            .source_revision()
            .max(minimum_revision.unwrap_or(fence.source_revision()));
        if before_revision < floor {
            return Err(WorkProgressError::SourceRevisionRegressed {
                role,
                observed: before_revision,
                minimum: floor,
            });
        }
        if observation.revision() < before_revision {
            return Err(WorkProgressError::SourceRevisionRegressed {
                role,
                observed: observation.revision(),
                minimum: before_revision,
            });
        }
    }
    Ok(())
}

fn validate_source_observation(
    role: WorkUnitRole,
    observation: &SourceRevisionObservation,
    fence: &StoredSourceFence,
    minimum_revision: Option<u64>,
) -> Result<(), WorkProgressError> {
    if observation.source_id() != fence.source_id()
        || observation.root_identity() != fence.root_identity()
        || observation.database_identity() != fence.database_identity()
        || observation.lifecycle_generation() != fence.lifecycle_generation()
        || observation.lease_epoch() != fence.lease_epoch()
    {
        return Err(WorkProgressError::SourceFenceMismatch { role });
    }
    let floor = fence
        .source_revision()
        .max(minimum_revision.unwrap_or(fence.source_revision()));
    if observation.revision() < floor {
        return Err(WorkProgressError::SourceRevisionRegressed {
            role,
            observed: observation.revision(),
            minimum: floor,
        });
    }
    Ok(())
}

fn same_stable_source_identity(left: &StoredSourceFence, right: &StoredSourceFence) -> bool {
    left.source_id() == right.source_id()
        && left.root_identity() == right.root_identity()
        && left.database_identity() == right.database_identity()
        && left.lifecycle_generation() == right.lifecycle_generation()
}

fn historical_scope_matches_current(
    current_epoch: u64,
    historical_epoch: u64,
    current: &ParticipantScope,
    historical: &ParticipantScope,
) -> bool {
    if current.participant_id != historical.participant_id || current.kind != historical.kind {
        return false;
    }
    if historical_epoch == current_epoch {
        return current == historical;
    }
    match (current.source_fence(), historical.source_fence()) {
        (Some(current), Some(historical)) => {
            same_stable_source_identity(current, historical)
                && historical.lease_epoch() <= current.lease_epoch()
                && if historical.lease_epoch() == current.lease_epoch() {
                    historical == current
                } else {
                    historical.source_revision() <= current.source_revision()
                }
        }
        (None, None) => true,
        _ => false,
    }
}

fn source_observation_for_evidence(
    evidence: &StoredStepEvidence,
) -> Option<&SourceRevisionObservation> {
    match evidence {
        StoredStepEvidence::SourceObserved { observation }
        | StoredStepEvidence::SourceCommitted {
            after: observation, ..
        } => Some(observation),
        StoredStepEvidence::Coordinator { receipt } => receipt.source_observation(),
        StoredStepEvidence::ReadinessScheduled { observation } => {
            Some(observation.source_observation())
        }
        StoredStepEvidence::Filesystem { .. } => None,
    }
}

fn validate_readiness_scheduled_binding(
    key: WorkUnitKey,
    context: &CompletionContext,
    evidence: &StoredStepEvidence,
) -> Result<(), WorkProgressError> {
    let StoredStepEvidence::ReadinessScheduled { observation } = evidence else {
        return Ok(());
    };
    if observation.readiness_key() != key {
        return Err(WorkProgressError::ReadinessEvidenceKeyMismatch {
            expected: key,
            actual: observation.readiness_key(),
        });
    }
    if observation.context() != context {
        return Err(WorkProgressError::ReadinessEvidenceContextMismatch { key });
    }
    Ok(())
}

fn validate_normal_completion_evidence(
    key: WorkUnitKey,
    context: &CompletionContext,
    evidence: &StoredStepEvidence,
    minimum_revision: Option<u64>,
) -> Result<(), WorkProgressError> {
    validate_evidence_for_role(key.role(), context.scope(), evidence, minimum_revision)?;
    validate_readiness_scheduled_binding(key, context, evidence)?;
    require_normal_readiness_evidence(key, evidence)
}

fn require_normal_readiness_evidence(
    key: WorkUnitKey,
    evidence: &StoredStepEvidence,
) -> Result<(), WorkProgressError> {
    if key.role() == WorkUnitRole::Readiness
        && !matches!(evidence, StoredStepEvidence::ReadinessScheduled { .. })
    {
        return Err(WorkProgressError::RoleEvidenceMismatch {
            role: WorkUnitRole::Readiness,
        });
    }
    Ok(())
}

fn same_stable_source_identity_observation(
    observation: &SourceRevisionObservation,
    fence: &StoredSourceFence,
) -> bool {
    observation.source_id() == fence.source_id()
        && observation.root_identity() == fence.root_identity()
        && observation.database_identity() == fence.database_identity()
        && observation.lifecycle_generation() == fence.lifecycle_generation()
}

fn same_stable_source_observation_identity(
    left: &SourceRevisionObservation,
    right: &SourceRevisionObservation,
) -> bool {
    left.source_id() == right.source_id()
        && left.root_identity() == right.root_identity()
        && left.database_identity() == right.database_identity()
        && left.lifecycle_generation() == right.lifecycle_generation()
}

fn validate_source_text(
    value: &str,
    field: ParticipantTextField,
    maximum: usize,
) -> Result<(), ProgressEvidenceError> {
    let violation = if value.is_empty() {
        Some(ParticipantTextViolation::Empty)
    } else if value.len() > maximum {
        Some(ParticipantTextViolation::TooLong)
    } else if value.chars().any(char::is_control) {
        Some(ParticipantTextViolation::ContainsControl)
    } else {
        None
    };
    violation.map_or(Ok(()), |violation| {
        Err(ProgressEvidenceError::InvalidSourceObservation(
            ParticipantValidationError::InvalidText { field, violation },
        ))
    })
}

/// Failure while validating bounded progress evidence.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum ProgressEvidenceError {
    /// A source observation failed the existing source-fence bounds.
    #[error("invalid source progress observation: {0}")]
    InvalidSourceObservation(#[source] ParticipantValidationError),
    /// A rich Readiness scheduling row failed its bounded observation shape.
    #[error("invalid Readiness scheduling evidence row: {0}")]
    ReadinessSchedulingObservation(#[source] Box<ReadinessSchedulingObservationError>),
}

/// Failure while deriving or advancing guarded work progress.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum WorkProgressError {
    /// The operation identity was nil and cannot fence a completion.
    #[error("work progress operation identity must be non-nil")]
    InvalidOperationId,
    /// The frozen plan or its initial topology was invalid.
    #[error("invalid work progress plan: {0}")]
    Plan(#[from] WorkPlanError),
    /// The participant inventory was invalid.
    #[error("invalid work progress participants: {0}")]
    Participants(#[from] ParticipantValidationError),
    /// The retained admission-claim observation did not exactly cover its capacity plan.
    #[error("invalid work progress admission claims: {0}")]
    AdmissionClaims(#[from] AdmissionClaimValidationError),
    /// A candidate claim-settlement observation failed bounded exact coverage validation.
    #[error("invalid work progress claim settlement: {0}")]
    ClaimSettlement(#[source] AdmissionClaimValidationError),
    /// A nested evidence observation was malformed.
    #[error("invalid work progress evidence: {0}")]
    Evidence(#[from] ProgressEvidenceError),
    /// The current frozen dispatch manifest could not be derived for an observation check.
    #[error("invalid work progress cancellation dispatch manifest: {0}")]
    CancellationDispatchManifest(#[source] CancellationDispatchManifestError),
    /// A cancellation dispatch outcome record failed local binding or refinement validation.
    #[error("invalid work progress cancellation dispatch outcome: {0}")]
    CancellationDispatchOutcome(#[source] CancellationDispatchOutcomeRecordError),
    /// A cancellation after-effect recovery inventory failed exact derivation or candidate
    /// validation.
    #[error("invalid work progress cancellation effect recovery inventory: {0}")]
    CancellationEffectInventory(#[source] CancellationEffectInventoryError),
    /// A cancellation-local cleanup-obligation manifest failed exact derivation or validation.
    #[error("invalid work progress cancellation cleanup manifest: {0}")]
    CancellationCleanupManifest(#[source] CancellationCleanupManifestError),
    /// A cancellation-local resource-cleanup manifest failed exact derivation or validation.
    #[error("invalid work progress cancellation resource-cleanup manifest: {0}")]
    CancellationResourceCleanupManifest(#[source] CancellationResourceCleanupManifestError),
    /// A cancellation-local resource-cleanup observation failed exact binding or context
    /// validation.
    #[error("invalid work progress cancellation resource-cleanup observation: {0}")]
    CancellationResourceCleanupObservation(
        #[source] CancellationResourceCleanupObservationRecordError,
    ),
    /// A cancellation-local namespace/disposition manifest failed exact derivation or validation.
    #[error("invalid work progress cancellation namespace-cleanup manifest: {0}")]
    CancellationNamespaceCleanupManifest(#[source] CancellationNamespaceCleanupManifestError),
    /// A cancellation-local namespace/disposition observation failed exact binding or context
    /// validation.
    #[error("invalid work progress cancellation namespace-cleanup observation: {0}")]
    CancellationNamespaceCleanupObservation(
        #[source] CancellationNamespaceCleanupObservationRecordError,
    ),
    /// A cancellation-local source-closure manifest failed exact derivation or validation.
    #[error("invalid work progress cancellation source-closure manifest: {0}")]
    CancellationSourceClosureManifest(#[source] SourceClosureManifestError),
    /// A cancellation-local source-closure observation failed exact binding or evidence
    /// validation.
    #[error("invalid work progress cancellation source-closure observation: {0}")]
    CancellationSourceClosureObservation(#[source] CancellationSourceClosureObservationRecordError),
    /// A cancellation-local coordinator-closure manifest failed exact derivation or validation.
    #[error("invalid work progress cancellation coordinator-closure manifest: {0}")]
    CancellationCoordinatorClosureManifest(#[source] CoordinatorClosureManifestError),
    /// A cancellation-local coordinator-closure observation failed exact binding or evidence
    /// validation.
    #[error("invalid work progress cancellation coordinator-closure observation: {0}")]
    CancellationCoordinatorClosureObservation(
        #[source] CancellationCoordinatorClosureObservationRecordError,
    ),
    /// A readiness deferral manifest failed exact planner derivation or validation.
    #[error("invalid work progress readiness deferral manifest: {0}")]
    ReadinessDeferralManifest(#[source] ReadinessDeferralManifestError),
    /// A readiness scheduling observation failed bounded exact validation.
    #[error("invalid work progress readiness scheduling observation: {0}")]
    ReadinessSchedulingObservation(#[source] ReadinessSchedulingObservationError),
    /// The readiness scheduling record was not captured at the current execution epoch.
    #[error(
        "work progress readiness scheduling observation epoch {actual} does not match expected epoch {expected}"
    )]
    ReadinessSchedulingEpochMismatch {
        /// Current execution epoch.
        expected: u64,
        /// Candidate record epoch.
        actual: u64,
    },
    /// A readiness row carried another execution epoch.
    #[error(
        "work progress readiness row {key:?} context epoch {actual} does not match expected epoch {expected}"
    )]
    ReadinessSchedulingContextEpochMismatch {
        /// Readiness key carrying the mismatched context.
        key: WorkUnitKey,
        /// Current execution epoch.
        expected: u64,
        /// Candidate context epoch.
        actual: u64,
    },
    /// A readiness row did not bind the current participant scope.
    #[error("work progress readiness row scope does not match {key:?}")]
    ReadinessSchedulingScopeMismatch {
        /// Readiness key carrying the mismatched scope.
        key: WorkUnitKey,
    },
    /// A readiness participant lacked the source fence required for scheduling observation.
    #[error("work progress readiness row has no source fence: {key:?}")]
    ReadinessSchedulingSourceFenceMissing {
        /// Readiness key whose scope is unfenced.
        key: WorkUnitKey,
    },
    /// The private readiness-deferral authority did not bind this operation or plan.
    #[error("work progress readiness deferral authority does not match")]
    ReadinessDeferralAuthorityMismatch,
    /// The private readiness-deferral authority carried another execution epoch.
    #[error(
        "work progress readiness deferral epoch {actual} does not match expected epoch {expected}"
    )]
    ReadinessDeferralEpochMismatch {
        /// Current progress epoch.
        expected: u64,
        /// Authority epoch.
        actual: u64,
    },
    /// The candidate did not follow the dense accepted deferral generation sequence.
    #[error(
        "work progress readiness deferral generation {actual} does not match expected generation {expected}"
    )]
    ReadinessDeferralGenerationMismatch {
        /// Expected prior or next generation.
        expected: u64,
        /// Supplied generation.
        actual: u64,
    },
    /// The accepted deferral generation could not advance without wrapping.
    #[error("work progress readiness deferral generation overflowed")]
    ReadinessDeferralGenerationOverflow,
    /// A compact historical batch repeated an artifact already deferred.
    #[error("work progress readiness deferral artifact {key:?} appears in more than one batch")]
    ReadinessDeferralArtifactOverlap {
        /// Repeated selected artifact.
        key: WorkUnitKey,
    },
    /// A compact batch did not contain the exact readiness rows selected by its artifacts.
    #[error("work progress readiness deferral rows do not cover the exact selected prerequisites")]
    ReadinessDeferralRowCoverageMismatch,
    /// An accepted compact batch did not leave its selected artifact Deferred.
    #[error("work progress readiness deferral artifact {key:?} is not Deferred")]
    ReadinessDeferralStatusMismatch {
        /// Artifact whose status was not Deferred.
        key: WorkUnitKey,
    },
    /// Deferred progress rows were not exactly explained by retained compact batches.
    #[error("work progress Deferred rows do not match retained readiness deferral batches")]
    ReadinessDeferralStatusCoverageMismatch,
    /// A selected artifact's required Readiness step was not completed with rich evidence.
    #[error("work progress readiness prerequisite {key:?} is not a completed rich observation")]
    ReadinessDeferralReadinessIncomplete {
        /// Readiness prerequisite key.
        key: WorkUnitKey,
    },
    /// A required Readiness completion was accepted after the deferral observation.
    #[error("work progress readiness prerequisite {key:?} was completed after its deferral")]
    ReadinessDeferralReadinessEpochInvalid {
        /// Readiness prerequisite key.
        key: WorkUnitKey,
    },
    /// A retained deferral row was not captured at its batch epoch.
    #[error(
        "work progress readiness deferral row {key:?} context epoch {actual} does not match batch epoch {expected}"
    )]
    ReadinessDeferralRowEpochMismatch {
        /// Readiness row key.
        key: WorkUnitKey,
        /// Row context epoch.
        actual: u64,
        /// Batch observation epoch.
        expected: u64,
    },
    /// A compact deferral batch epoch regressed or exceeded current progress.
    #[error("work progress readiness deferral epoch {epoch} is outside [{previous}, {current}]")]
    ReadinessDeferralEpochInvalid {
        /// Candidate batch epoch.
        epoch: u64,
        /// Previous accepted epoch.
        previous: u64,
        /// Current progress epoch.
        current: u64,
    },
    /// A current candidate was not captured at the authority's current epoch.
    #[error("work progress readiness deferral candidate epoch does not match current progress")]
    ReadinessDeferralCandidateEpochMismatch,
    /// A candidate selected a non-pending optional artifact or changed a completed step.
    #[error("work progress readiness deferral artifact {key:?} is not pending optional work")]
    ReadinessDeferralArtifactNotPending {
        /// Artifact whose status or role was invalid.
        key: WorkUnitKey,
    },
    /// A current candidate's readiness row violated publication identity ordering.
    #[error("work progress readiness deferral publication identity conflicts for {key:?}")]
    ReadinessDeferralPublicationConflict {
        /// Readiness key whose identity fields changed.
        key: WorkUnitKey,
    },
    /// A current candidate lowered a readiness source generation.
    #[error("work progress readiness deferral source generation regressed for {key:?}")]
    ReadinessDeferralSourceGenerationRegression {
        /// Readiness key whose source generation regressed.
        key: WorkUnitKey,
    },
    /// A current candidate reused or lowered a readiness publication revision.
    #[error("work progress readiness deferral publication revision did not advance for {key:?}")]
    ReadinessDeferralRevisionRegression {
        /// Readiness key whose publication revision regressed.
        key: WorkUnitKey,
    },
    /// A retained readiness source observation regressed below causally prior proof.
    #[error(
        "work progress readiness deferral source revision {observed} regressed below {minimum} for {key:?}"
    )]
    ReadinessDeferralSourceRevisionRegression {
        /// Readiness key carrying the regressed source observation.
        key: WorkUnitKey,
        /// Observed source revision.
        observed: u64,
        /// Greatest causally prior revision.
        minimum: u64,
    },
    /// The private after-effect reconciliation authority did not bind this operation or plan.
    #[error("work progress after-effect reconciliation authority does not match")]
    AfterEffectReconciliationAuthorityMismatch,
    /// The private after-effect reconciliation authority carried another execution epoch.
    #[error(
        "work progress after-effect reconciliation epoch {actual} does not match expected epoch {expected}"
    )]
    AfterEffectReconciliationEpochMismatch {
        /// Current execution epoch required by progress.
        expected: u64,
        /// Epoch supplied by the authority.
        actual: u64,
    },
    /// The retained dispatch record was not captured at the current owner epoch.
    #[error(
        "work progress after-effect dispatch epoch {actual} does not match expected epoch {expected}"
    )]
    AfterEffectReconciliationDispatchEpochMismatch {
        /// Current execution epoch required by progress.
        expected: u64,
        /// Epoch supplied by the dispatch record.
        actual: u64,
    },
    /// The retained source-closure record was not captured at the current owner epoch.
    #[error(
        "work progress after-effect source-closure epoch {actual} does not match expected epoch {expected}"
    )]
    AfterEffectReconciliationSourceClosureEpochMismatch {
        /// Current execution epoch required by progress.
        expected: u64,
        /// Epoch supplied by the source-closure record.
        actual: u64,
    },
    /// The retained coordinator-closure record was not captured at the current owner epoch.
    #[error(
        "work progress after-effect coordinator-closure epoch {actual} does not match expected epoch {expected}"
    )]
    AfterEffectReconciliationCoordinatorClosureEpochMismatch {
        /// Current execution epoch required by progress.
        expected: u64,
        /// Epoch supplied by the coordinator-closure record.
        actual: u64,
    },
    /// A retained after-effect state did not bind the frozen cancellation inventory.
    #[error("work progress after-effect reconciliation cancellation inventory does not match")]
    AfterEffectReconciliationInventoryMismatch,
    /// The accepted after-effect generation could not advance without wrapping.
    #[error("work progress after-effect reconciliation generation overflowed")]
    AfterEffectReconciliationGenerationOverflow,
    /// The authority expected another accepted after-effect generation.
    #[error(
        "work progress after-effect reconciliation generation {actual} does not match expected generation {expected}"
    )]
    AfterEffectReconciliationGenerationMismatch {
        /// Generation currently retained by progress.
        expected: u64,
        /// Generation supplied by the authority.
        actual: u64,
    },
    /// A changed after-effect dispatch record did not advance with the accepted state.
    #[error(
        "work progress after-effect dispatch generation {actual} does not match expected generation {expected}"
    )]
    AfterEffectReconciliationDispatchGenerationMismatch {
        /// Generation required for a changed accepted state.
        expected: u64,
        /// Generation supplied by the dispatch record.
        actual: u64,
    },
    /// The cancellation route or disposition cannot retain this after-effect observation.
    #[error("work progress after-effect reconciliation state is invalid")]
    AfterEffectReconciliationStateInvalid,
    /// An audit overlay is active and must be resolved before accepting after-effect state.
    #[error("work progress after-effect reconciliation cannot be accepted while audit is active")]
    AfterEffectReconciliationAuditActive,
    /// A late source or coordinator receipt was retained while dispatch remained uncertain.
    #[error(
        "work progress after-effect reconciliation requires determinate dispatch before retaining late receipts"
    )]
    AfterEffectReconciliationDispatchIndeterminate,
    /// A coordinator receipt was retained before every required source receipt was observed.
    #[error(
        "work progress after-effect reconciliation requires all source closures before coordinator receipts"
    )]
    AfterEffectReconciliationSourceClosureIncomplete,
    /// A candidate changed an already observed source or coordinator receipt.
    #[error("work progress after-effect reconciliation receipt replay conflicts")]
    AfterEffectReconciliationReceiptReplayConflict,
    /// The private cleanup-observation authority did not bind this operation or plan.
    #[error("work progress cancellation cleanup observation authority does not match")]
    CancellationCleanupAuthorityMismatch,
    /// The private cleanup-observation authority carried another execution epoch.
    #[error(
        "work progress cancellation cleanup observation epoch {actual} does not match expected epoch {expected}"
    )]
    CancellationCleanupEpochMismatch {
        /// Current execution epoch required by progress.
        expected: u64,
        /// Epoch supplied by the authority.
        actual: u64,
    },
    /// Cleanup observations cannot be retained while an audit overlay is active.
    #[error(
        "work progress cancellation cleanup observation cannot be accepted while audit is active"
    )]
    CancellationCleanupAuditActive,
    /// The current cancellation route or nonterminal disposition does not match the authority.
    #[error("work progress cancellation cleanup observation route or disposition is invalid")]
    CancellationCleanupStateInvalid,
    /// The private cleanup-observation authority did not bind the frozen cancellation inventory.
    #[error("work progress cancellation cleanup observation inventory does not match")]
    CancellationCleanupInventoryMismatch,
    /// Cleanup observations require an accepted after-effect reconciliation state.
    #[error("work progress cancellation cleanup observation requires accepted after-effect state")]
    CancellationCleanupAfterEffectMissing,
    /// The authority did not bind the exact currently accepted after-effect state.
    #[error("work progress cancellation cleanup observation after-effect state does not match")]
    CancellationCleanupAfterEffectMismatch,
    /// The linked after-effect state was not accepted at the current epoch.
    #[error(
        "work progress cancellation cleanup after-effect epoch {actual} does not match expected epoch {expected}"
    )]
    CancellationCleanupAfterEffectEpochMismatch {
        /// Current execution epoch required by progress.
        expected: u64,
        /// Epoch carried by the linked after-effect state.
        actual: u64,
    },
    /// The resource-cleanup record was not captured at the current epoch.
    #[error(
        "work progress cancellation resource-cleanup observation epoch {actual} does not match expected epoch {expected}"
    )]
    CancellationCleanupResourceEpochMismatch {
        /// Current execution epoch required by progress.
        expected: u64,
        /// Epoch carried by the resource record.
        actual: u64,
    },
    /// The namespace-cleanup record was not captured at the current epoch.
    #[error(
        "work progress cancellation namespace-cleanup observation epoch {actual} does not match expected epoch {expected}"
    )]
    CancellationCleanupNamespaceEpochMismatch {
        /// Current execution epoch required by progress.
        expected: u64,
        /// Epoch carried by the namespace record.
        actual: u64,
    },
    /// The private cleanup authority expected another accepted global generation.
    #[error(
        "work progress cancellation cleanup generation {actual} does not match expected generation {expected}"
    )]
    CancellationCleanupGenerationMismatch {
        /// Current global recovery generation required by progress.
        expected: u64,
        /// Generation supplied by the authority.
        actual: u64,
    },
    /// The global recovery generation could not advance without wrapping.
    #[error("work progress cancellation cleanup generation overflowed")]
    CancellationCleanupGenerationOverflow,
    /// A changed resource or namespace record did not advance its observation generation.
    #[error(
        "work progress cancellation cleanup observation generation {actual} is not newer than {previous}"
    )]
    CancellationCleanupObservationGenerationRegression {
        /// Previously accepted record generation.
        previous: u64,
        /// Candidate record generation.
        actual: u64,
    },
    /// An accepted observed cleanup row was removed or rewritten as pending.
    #[error("work progress cancellation cleanup observation replay conflicts for {key:?}")]
    CancellationCleanupObservationReplayConflict {
        /// Key whose accepted observation was regressed.
        key: WorkUnitKey,
    },
    /// A changed cleanup observation did not advance its owner-local revision.
    #[error(
        "work progress cancellation cleanup owner revision {actual} for {key:?} is not newer than {previous}"
    )]
    CancellationCleanupOwnerRevisionRegression {
        /// Key whose owner observation regressed.
        key: WorkUnitKey,
        /// Previously accepted owner revision.
        previous: u64,
        /// Candidate owner revision.
        actual: u64,
    },
    /// A newly observed or refreshed cleanup row did not carry current owner context.
    #[error(
        "work progress cancellation cleanup observation context epoch {actual} for {key:?} does not match expected epoch {expected}"
    )]
    CancellationCleanupObservationEpochMismatch {
        /// Current execution epoch required by progress.
        expected: u64,
        /// Epoch carried by the candidate context.
        actual: u64,
        /// Key carrying the candidate context.
        key: WorkUnitKey,
    },
    /// A prepublication authority attempted to bypass accepted after-effect state.
    #[error(
        "work progress prepublication cancellation cannot bypass accepted after-effect reconciliation"
    )]
    PrePublishCancellationAfterEffectReconciliationActive,
    /// A prepublication cleanup observation failed its bounded local shape validation.
    #[error("invalid work progress prepublication cleanup: {0}")]
    PrePublishCleanup(#[source] PrePublishCleanupValidationError),
    /// An audit target was malformed or absent from the frozen plan.
    #[error("invalid work progress audit target: {0}")]
    AuditTarget(#[from] AuditTargetValidationError),
    /// A retry observation failed its bounded validation.
    #[error("invalid work progress retry observation: {0}")]
    RetryObservation(#[source] RetryObservationValidationError),
    /// The retry lease contains no eligible instant before its exclusive expiry.
    #[error("work progress retry lease window is empty")]
    EmptyRetryWindow,
    /// A retry observation replayed an attempt that is not newer than the retained attempt.
    #[error("work progress retry attempt {attempt} is not newer than the retained attempt")]
    RetryReplayConflict {
        /// Attempt number rejected as a replay or regression.
        attempt: u32,
    },
    /// Work was asked to resume without a pending retry observation.
    #[error("work progress retry is not pending")]
    RetryResumeNotPending,
    /// A private retry-resume authority did not bind the operation, plan, or observation.
    #[error("work progress retry-resume authority does not match")]
    RetryResumeAuthorityMismatch,
    /// A private retry-resume authority carried another execution epoch.
    #[error("work progress retry-resume epoch {actual} does not match expected epoch {expected}")]
    RetryResumeEpochMismatch {
        /// Current execution epoch required by progress.
        expected: u64,
        /// Epoch supplied by the resume authority.
        actual: u64,
    },
    /// The retry resume time was outside the retained lease window.
    #[error("work progress retry-resume time {now} is outside [{next_retry}, {lease_expiry})")]
    RetryResumeWindowInvalid {
        /// Time observed by the owner while reacquiring the retry lease.
        now: i64,
        /// Earliest permitted retry time.
        next_retry: i64,
        /// Exclusive lease expiry boundary.
        lease_expiry: i64,
    },
    /// The execution epoch could not be advanced without wrapping.
    #[error("work progress execution epoch overflowed")]
    ExecutionEpochOverflow,
    /// A completion was issued for another execution epoch.
    #[error("work progress completion epoch {actual} does not match expected epoch {expected}")]
    ExecutionEpochMismatch {
        /// Current execution epoch required by progress.
        expected: u64,
        /// Epoch supplied by the completion authority.
        actual: u64,
    },
    /// A private claim-settlement authority carried another execution epoch.
    #[error(
        "work progress claim-settlement epoch {actual} does not match expected epoch {expected}"
    )]
    ClaimSettlementEpochMismatch {
        /// Current execution epoch required by progress.
        expected: u64,
        /// Epoch supplied by the settlement authority.
        actual: u64,
    },
    /// A private source-reacquisition authority did not bind the operation, plan, or old scope.
    #[error("work progress source-reacquisition authority does not match")]
    SourceReacquisitionAuthorityMismatch,
    /// The source-reacquisition target was not a currently fenced source database participant.
    #[error(
        "work progress source-reacquisition target is not a fenced source database: {participant_id}"
    )]
    SourceReacquisitionTargetInvalid {
        /// Participant named by the private reacquisition authority.
        participant_id: ParticipantId,
    },
    /// A private source-reacquisition authority carried another execution epoch.
    #[error(
        "work progress source-reacquisition epoch {actual} does not match expected epoch {expected}"
    )]
    SourceReacquisitionEpochMismatch {
        /// Current execution epoch required by progress.
        expected: u64,
        /// Epoch supplied by the reacquisition authority.
        actual: u64,
    },
    /// A reacquired fence changed stable source identity or lifecycle.
    #[error("work progress source-reacquisition stable source identity does not match")]
    SourceReacquisitionIdentityMismatch,
    /// A reacquired lease was not strictly newer than every scope for the source.
    #[error(
        "work progress source-reacquisition lease epoch {observed} is not newer than {current}"
    )]
    SourceReacquisitionLeaseRegressed {
        /// Lease epoch carried by the new source fence.
        observed: u64,
        /// Greatest lease epoch already observed for the stable source.
        current: u64,
    },
    /// A reacquired source revision fell behind accepted source or coordinator evidence.
    #[error("work progress source-reacquisition revision {observed} regressed below {minimum}")]
    SourceReacquisitionRevisionRegressed {
        /// Revision carried by the new source fence.
        observed: u64,
        /// Greatest accepted revision for the stable source.
        minimum: u64,
    },
    /// The source-reacquisition epoch could not advance without wrapping.
    #[error("work progress source-reacquisition execution epoch overflowed")]
    SourceReacquisitionEpochOverflow,
    /// A generic audit was requested before filesystem publication.
    #[error("work progress audit is only valid after filesystem publication")]
    AuditPhaseInvalid,
    /// A new audit request changed the currently active reason or target.
    #[error("work progress audit replay conflicts with the active observation")]
    AuditReplayConflict,
    /// The disposition beneath an audit cannot be restored by this overlay.
    #[error("work progress audit cannot suspend disposition {disposition:?}")]
    AuditUnderlyingDispositionUnsupported {
        /// Disposition that would have been hidden by the audit.
        disposition: FullProgressDisposition,
    },
    /// The retained audit generation could not advance without wrapping.
    #[error("work progress audit generation overflowed")]
    AuditGenerationOverflow,
    /// The progress disposition says audit is active but has no retained overlay.
    #[error("work progress audit overlay is missing")]
    AuditStateMissing,
    /// A private audit-resolution authority was used without an active audit.
    #[error("work progress audit resolution is not active")]
    AuditResolutionNotActive,
    /// A private audit-resolution authority did not bind the operation or plan.
    #[error("work progress audit-resolution authority does not match")]
    AuditResolutionAuthorityMismatch,
    /// A private audit-resolution authority carried another execution epoch.
    #[error(
        "work progress audit-resolution epoch {actual} does not match expected epoch {expected}"
    )]
    AuditResolutionEpochMismatch {
        /// Current execution epoch required by progress.
        expected: u64,
        /// Epoch supplied by the resolution authority.
        actual: u64,
    },
    /// A private audit-resolution authority carried another retained generation.
    #[error(
        "work progress audit-resolution generation {actual} does not match expected generation {expected}"
    )]
    AuditResolutionGenerationMismatch {
        /// Current active audit generation.
        expected: u64,
        /// Generation supplied by the resolution authority.
        actual: u64,
    },
    /// A private audit-resolution authority carried another reason or target.
    #[error("work progress audit-resolution observation does not match")]
    AuditResolutionObservationMismatch,
    /// The suspended retry or cancellation hold lacks its retained observation.
    #[error("work progress audit suspended hold observation is missing")]
    AuditSuspendedHoldMissing,
    /// A cancellation observation failed its bounded validation.
    #[error("invalid work progress cancellation observation: {0}")]
    CancellationObservation(#[source] CancellationRequestValidationError),
    /// A plan unit had no captured participant scope.
    #[error("work progress participant scope is missing for {participant_id}")]
    ParticipantScopeMissing {
        /// Participant whose scope was absent.
        participant_id: ParticipantId,
    },
    /// The completion named another operation.
    #[error("work progress completion operation identity does not match")]
    OperationIdMismatch,
    /// The completion carried another exact frozen plan.
    #[error("work progress completion plan does not match")]
    PlanMismatch,
    /// The completion key was not in the frozen plan.
    #[error("work progress completion key is unknown: {key:?}")]
    UnknownWorkUnit {
        /// Key absent from the frozen plan.
        key: WorkUnitKey,
    },
    /// The completion's participant scope did not match its exact key.
    #[error("work progress completion participant scope does not match {key:?}")]
    ParticipantScopeMismatch {
        /// Completion key carrying the mismatched scope.
        key: WorkUnitKey,
    },
    /// The completion attempted to apply to a non-pending step.
    #[error("work progress step is not pending: {key:?}")]
    StepNotPending {
        /// Key whose state was already advanced or deferred.
        key: WorkUnitKey,
    },
    /// A prerequisite key was absent from the frozen plan.
    #[error("work progress prerequisite is missing: {edge:?}")]
    PrerequisiteMissing {
        /// Edge carrying the missing endpoint.
        edge: WorkPrerequisite,
    },
    /// A prerequisite has not completed.
    #[error("work progress prerequisite {prerequisite:?} is incomplete for {dependent:?}")]
    PrerequisiteIncomplete {
        /// Incomplete prerequisite key.
        prerequisite: WorkUnitKey,
        /// Dependent key awaiting the prerequisite.
        dependent: WorkUnitKey,
    },
    /// A repeated key carried changed evidence.
    #[error("work progress completion replay conflicts for {key:?}")]
    CompletionReplayConflict {
        /// Key whose replay evidence changed.
        key: WorkUnitKey,
    },
    /// A completed step had no accepted historical context.
    #[error("work progress completed step has no accepted context: {key:?}")]
    CompletionContextMissing {
        /// Key whose accepted context was absent.
        key: WorkUnitKey,
    },
    /// A completed step had no retained evidence.
    #[error("work progress completed step has no retained evidence: {key:?}")]
    CompletionEvidenceMissing {
        /// Key whose retained evidence was absent.
        key: WorkUnitKey,
    },
    /// A non-completed step carried an accepted historical context.
    #[error("work progress non-completed step has unexpected context: {key:?}")]
    CompletionContextUnexpected {
        /// Key carrying the unexpected context.
        key: WorkUnitKey,
    },
    /// A non-completed step carried retained completion evidence.
    #[error("work progress non-completed step has unexpected evidence: {key:?}")]
    CompletionEvidenceUnexpected {
        /// Key carrying the unexpected evidence.
        key: WorkUnitKey,
    },
    /// A retained completion context did not match the current participant identity or kind.
    #[error("work progress completion context scope does not match {key:?}")]
    CompletionContextScopeMismatch {
        /// Key carrying the mismatched historical context.
        key: WorkUnitKey,
    },
    /// A retained completion context belonged to a future execution epoch.
    #[error(
        "work progress completion context epoch {actual} is newer than current epoch {current} for {key:?}"
    )]
    CompletionContextEpochFuture {
        /// Key carrying the future context.
        key: WorkUnitKey,
        /// Historical epoch observed on the malformed context.
        actual: u64,
        /// Current epoch retained by progress.
        current: u64,
    },
    /// A historical source observation exceeded the currently rebound fence baseline.
    #[error(
        "work progress completion history revision {observed} exceeds current fence revision {current_revision} for {key:?}"
    )]
    CompletionHistoryRevisionBeyondCurrentFence {
        /// Key carrying the historical observation.
        key: WorkUnitKey,
        /// Revision retained by the historical evidence.
        observed: u64,
        /// Current source-fence baseline.
        current_revision: u64,
    },
    /// The evidence role did not match the exact work role.
    #[error("work progress evidence role does not match {role:?}")]
    RoleEvidenceMismatch {
        /// Work role expected by the key.
        role: WorkUnitRole,
    },
    /// A rich normal Readiness receipt named a different work-unit key.
    #[error(
        "work progress Readiness evidence key {actual:?} does not match enclosing key {expected:?}"
    )]
    ReadinessEvidenceKeyMismatch {
        /// Key carried by the enclosing progress completion.
        expected: WorkUnitKey,
        /// Key carried by the embedded Readiness scheduling row.
        actual: WorkUnitKey,
    },
    /// A rich normal Readiness receipt carried a different completion context.
    #[error("work progress Readiness evidence context does not match {key:?}")]
    ReadinessEvidenceContextMismatch {
        /// Key carrying the mismatched embedded context.
        key: WorkUnitKey,
    },
    /// The participant kind did not match the exact work role.
    #[error("work progress participant kind for {role:?} is {actual:?}, expected {expected:?}")]
    ParticipantKindMismatch {
        /// Work role requiring a participant kind.
        role: WorkUnitRole,
        /// Required participant kind.
        expected: ParticipantKind,
        /// Captured participant kind.
        actual: ParticipantKind,
    },
    /// A source role had no captured source fence.
    #[error("work progress source fence is missing for {role:?}")]
    SourceFenceMissing {
        /// Source role requiring a fence.
        role: WorkUnitRole,
    },
    /// A source completion changed its captured identity or lease boundary.
    #[error("work progress source fence does not match for {role:?}")]
    SourceFenceMismatch {
        /// Source role carrying the mismatched fence.
        role: WorkUnitRole,
    },
    /// A source-scoped coordinator receipt omitted its source revision observation.
    #[error("work progress coordinator source observation is missing for {role:?}")]
    CoordinatorSourceObservationMissing {
        /// Coordinator role requiring the source observation.
        role: WorkUnitRole,
    },
    /// An unfenced coordinator receipt carried a source revision observation.
    #[error("work progress coordinator source observation is forbidden for {role:?}")]
    CoordinatorSourceObservationForbidden {
        /// Unfenced coordinator role carrying the source observation.
        role: WorkUnitRole,
    },
    /// A source completion moved behind the last accepted revision.
    #[error("work progress source revision {observed} regressed below {minimum} for {role:?}")]
    SourceRevisionRegressed {
        /// Role carrying the regressed observation.
        role: WorkUnitRole,
        /// Observed revision.
        observed: u64,
        /// Minimum retained revision.
        minimum: u64,
    },
    /// A coordinator completion still carried a deficit.
    #[error("work progress coordinator deficit remains for {role:?}")]
    CoordinatorDeficitPresent {
        /// Coordinator role carrying the unresolved deficit.
        role: WorkUnitRole,
    },
    /// A retry, audit, or cancellation disposition stopped completion application.
    #[error("work progress is suspended by disposition {disposition:?}")]
    ProgressSuspended {
        /// Resumable disposition currently blocking progress.
        disposition: FullProgressDisposition,
    },
    /// A terminal disposition cannot be changed.
    #[error("work progress terminal transition is already durable")]
    TerminalTransitionForbidden,
    /// A claim-settlement authority did not bind this operation or frozen plan.
    #[error("work progress claim-settlement authority does not match")]
    ClaimSettlementAuthorityMismatch,
    /// No admission-claim observation was retained for settlement.
    #[error("work progress claim-settlement admission claims are missing")]
    ClaimSettlementAdmissionClaimsMissing,
    /// A claim-settlement authority carried a different retained admission inventory.
    #[error("work progress claim-settlement admission claims do not match")]
    ClaimSettlementClaimsMismatch,
    /// Required work or an optional artifact decision is still pending.
    #[error("work progress claim settlement requires all work and artifacts to be resolved")]
    ClaimSettlementProgressIncomplete,
    /// A replay attempted to change an already-retained settlement observation.
    #[error("work progress claim-settlement replay conflicts with the retained observation")]
    ClaimSettlementReplayConflict,
    /// The selected key was not an optional rebuildable artifact still pending.
    #[error("work progress artifact deferral is forbidden for {key:?}")]
    ArtifactDeferralForbidden {
        /// Key that could not be deferred.
        key: WorkUnitKey,
    },
    /// A second cancellation request changed the captured request.
    #[error("work progress cancellation replay conflicts")]
    CancellationReplayConflict,
    /// Cancellation cannot become terminal until cleanup, settlement, and safe-state authorities
    /// are available.
    #[error("work progress cancellation outcome authority is not yet available")]
    CancellationOutcomeUnavailable,
    /// Cancellation requires an exact retained admission-claim observation.
    #[error("work progress cancellation admission claims are missing")]
    CancellationAdmissionClaimsMissing,
    /// An existing cancellation request had no frozen inventory to replay.
    #[error("work progress cancellation inventory is missing")]
    CancellationInventoryMissing,
    /// The cancellation generation could not advance without wrapping.
    #[error("work progress cancellation generation overflowed")]
    CancellationGenerationOverflow,
    /// The bounded cancellation inventory was invalid or did not bind this progress.
    #[error("invalid work progress cancellation inventory: {0}")]
    CancellationInventory(#[from] CancellationInventoryError),
    /// Cancellation was requested from an unsupported phase.
    #[error("work progress cancellation phase is invalid")]
    CancellationPhaseInvalid,
    /// A cancellation terminal transition had no matching request.
    #[error("work progress cancellation was not requested")]
    CancellationNotRequested,
    /// The private cancellation gate did not bind this operation and plan.
    #[error("work progress cancellation authority does not match")]
    CancellationAuthorityMismatch,
    /// A private prepublication cancellation authority did not bind this operation or plan.
    #[error("work progress prepublication cancellation authority does not match")]
    PrePublishCancellationAuthorityMismatch,
    /// A private prepublication cancellation authority carried another execution epoch.
    #[error(
        "work progress prepublication cancellation epoch {actual} does not match expected epoch {expected}"
    )]
    PrePublishCancellationEpochMismatch {
        /// Current execution epoch required by progress.
        expected: u64,
        /// Epoch supplied by the cleanup authority.
        actual: u64,
    },
    /// The private prepublication cancellation authority carried a dispatch record from
    /// another execution epoch.
    #[error(
        "work progress prepublication cancellation dispatch epoch {actual} does not match expected epoch {expected}"
    )]
    PrePublishCancellationDispatchEpochMismatch {
        /// Current execution epoch required by progress.
        expected: u64,
        /// Epoch supplied by the dispatch outcome record.
        actual: u64,
    },
    /// A new or changed dispatch outcome record did not advance with the recovery observation.
    #[error(
        "work progress prepublication cancellation dispatch generation {actual} does not match expected generation {expected}"
    )]
    PrePublishCancellationDispatchGenerationMismatch {
        /// Record generation required for a new or changed outcome record.
        expected: u64,
        /// Generation supplied by the outcome record.
        actual: u64,
    },
    /// A private prepublication cancellation authority carried another cleanup generation.
    #[error(
        "work progress prepublication cancellation generation {actual} does not match expected generation {expected}"
    )]
    PrePublishCancellationRecoveryGenerationMismatch {
        /// Current cleanup-observation generation.
        expected: u64,
        /// Generation supplied by the cleanup authority.
        actual: u64,
    },
    /// The cleanup-observation generation could not advance without wrapping.
    #[error("work progress prepublication cancellation generation overflowed")]
    PrePublishCancellationRecoveryGenerationOverflow,
    /// No admission-claim observation was retained for prepublication cancellation.
    #[error("work progress prepublication cancellation admission claims are missing")]
    PrePublishCancellationAdmissionClaimsMissing,
    /// The cleanup authority carried a different frozen cancellation inventory.
    #[error("work progress prepublication cancellation inventory does not match")]
    PrePublishCancellationInventoryMismatch,
    /// The cleanup authority carried a route other than before-publication.
    #[error("work progress prepublication cancellation route is invalid: {route:?}")]
    PrePublishCancellationRouteInvalid {
        /// Route captured by the cancellation inventory.
        route: CancellationRoute,
    },
    /// Progress is not in a before-publication cancellation state.
    #[error("work progress prepublication cancellation state is invalid")]
    PrePublishCancellationStateInvalid,
    /// An audit overlay is active and must be resolved by its own authority first.
    #[error("work progress prepublication cancellation cannot finish while audit is active")]
    PrePublishCancellationAuditActive,
    /// A prepublication settlement observation failed exact claim coverage validation.
    #[error("invalid work progress prepublication cancellation settlement: {0}")]
    PrePublishCancellationSettlement(#[source] AdmissionClaimValidationError),
    /// A bounded unstarted-action quiescence observation failed local shape validation.
    #[error("invalid work progress prepublication unstarted quiescence: {0}")]
    PrePublishUnstartedQuiescence(#[source] PrePublishUnstartedQuiescenceValidationError),
    /// A closed claim outcome does not match its allocation class policy.
    #[error(
        "work progress prepublication settlement outcome {outcome:?} is incompatible with claim {claim_id} class {allocation_class:?}"
    )]
    PrePublishCancellationSettlementClassMismatch {
        /// Claim carrying the incompatible outcome.
        claim_id: AdmissionClaimId,
        /// Class whose policy rejected the outcome.
        allocation_class: super::full_capacity::FullCapacityAllocationClass,
        /// Outcome observed for the claim.
        outcome: AdmissionClaimSettlementOutcome,
    },
    /// A released staging or recovery claim lacks a matching observed absence.
    #[error(
        "work progress prepublication settlement class {allocation_class:?} requires cleanup absence"
    )]
    PrePublishCancellationSettlementCleanupIncomplete {
        /// Class whose release is unsafe while cleanup is incomplete.
        allocation_class: super::full_capacity::FullCapacityAllocationClass,
    },
    /// A non-held claim settlement was submitted before every dispatch row was proven
    /// `NeverEntered`.
    #[error(
        "work progress prepublication cancellation dispatch is incomplete; non-held settlement is not authorized"
    )]
    PrePublishCancellationDispatchIncomplete,
    /// A non-held settlement for a retained claim was submitted before the unstarted-action
    /// quiescence gate was complete.
    #[error(
        "work progress prepublication cancellation settlement requires dispatch and unstarted quiescence completeness"
    )]
    PrePublishCancellationSettlementGateIncomplete,
    /// A changed settlement attempted to rewrite an already settled claim.
    #[error("work progress prepublication settlement replay conflicts with a settled claim")]
    PrePublishCancellationSettlementReplayConflict,
    /// A cleanup row named an action that was not entered in the frozen prepublication inventory.
    #[error("work progress prepublication cleanup action {action_ordinal} is not cleanable")]
    PrePublishCleanupActionSetMismatch {
        /// Action named by the invalid cleanup row.
        action_ordinal: u8,
    },
    /// A cleanup row carried the wrong origin-state variant for its immutable action policy.
    #[error(
        "work progress prepublication cleanup origin state is invalid for action {action_ordinal}"
    )]
    PrePublishCleanupOriginalStateMismatch {
        /// Action whose immutable origin policy was violated.
        action_ordinal: u8,
    },
    /// A cleanup row supplied a staging absence for an action without staging work.
    #[error(
        "work progress prepublication cleanup staging absence is unexpected for action {action_ordinal}"
    )]
    PrePublishCleanupStagingUnexpected {
        /// Action carrying an unexpected staging field.
        action_ordinal: u8,
    },
    /// A cleanup row supplied a recovery-payload absence for an action without recovery work.
    #[error(
        "work progress prepublication cleanup recovery absence is unexpected for action {action_ordinal}"
    )]
    PrePublishCleanupRecoveryUnexpected {
        /// Action carrying an unexpected recovery field.
        action_ordinal: u8,
    },
    /// A quiescence row named an action that is unknown or not still `NotStarted`.
    #[error(
        "work progress prepublication unstarted quiescence action {action_ordinal} is not eligible"
    )]
    PrePublishUnstartedQuiescenceActionSetMismatch {
        /// Action named by the invalid quiescence row.
        action_ordinal: u8,
    },
    /// A quiescence row did not carry the exact pending key set for its action.
    #[error(
        "work progress prepublication unstarted quiescence keys do not match action {action_ordinal}"
    )]
    PrePublishUnstartedQuiescenceKeySetMismatch {
        /// Action whose key set was invalid.
        action_ordinal: u8,
    },
    /// A quiescence row supplied staging absence for an action without staging work.
    #[error(
        "work progress prepublication unstarted quiescence staging absence is unexpected for action {action_ordinal}"
    )]
    PrePublishUnstartedQuiescenceStagingUnexpected {
        /// Action carrying an unexpected staging field.
        action_ordinal: u8,
    },
    /// A quiescence row supplied recovery absence without a recovery obligation.
    #[error(
        "work progress prepublication unstarted quiescence recovery absence is unexpected for action {action_ordinal}"
    )]
    PrePublishUnstartedQuiescenceRecoveryUnexpected {
        /// Action carrying an unexpected recovery field.
        action_ordinal: u8,
    },
    /// A later observation attempted to regress decisive quiescence or absence evidence.
    #[error(
        "work progress prepublication unstarted quiescence replay conflicts with a decisive observation"
    )]
    PrePublishCancellationQuiescenceReplayConflict,
}

impl From<RetryObservationValidationError> for WorkProgressError {
    fn from(error: RetryObservationValidationError) -> Self {
        Self::RetryObservation(error)
    }
}

impl From<CancellationRequestValidationError> for WorkProgressError {
    fn from(error: CancellationRequestValidationError) -> Self {
        Self::CancellationObservation(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::durable_capacity::{PROTECTED_FREE_SPACE_FLOOR, VolumeIdentity};
    use crate::operation_journal::MAX_CANCELLATION_STEPS;
    use crate::operation_journal::{
        AdmissionClaim, AdmissionClaimId, AdmissionClaimInventory, AdmissionClaimSettlement,
        AdmissionClaimSettlementInventory, AdmissionClaimSettlementOutcome,
        AdmissionClaimValidationError, CancellationActionState, CancellationCleanupManifest,
        CancellationCleanupManifestValidationError, CancellationCleanupSegment,
        CancellationCoordinatorClosureObservationRecord,
        CancellationCoordinatorClosureObservationRecordError,
        CancellationCoordinatorClosureObservationRow, CancellationCoordinatorClosureOutcome,
        CancellationDispatchManifest, CancellationDispatchManifestError,
        CancellationDispatchManifestValidationError, CancellationDispatchOutcome,
        CancellationDispatchOutcomeRecord, CancellationDispatchOutcomeRecordError,
        CancellationDispatchOutcomeRow, CancellationEffectBundle, CancellationEffectFactKind,
        CancellationEffectOutcome, CancellationEffectRecipe, CancellationEffectRecoveryInventory,
        CancellationNamespaceCleanupManifest, CancellationNamespaceCleanupObservationRecord,
        CancellationNamespaceCleanupObservationRecordError,
        CancellationNamespaceCleanupObservationRow, CancellationNamespaceCleanupOutcome,
        CancellationResourceCleanupManifest, CancellationResourceCleanupManifestValidationError,
        CancellationResourceCleanupObservationRecord,
        CancellationResourceCleanupObservationRecordError,
        CancellationResourceCleanupObservationRow, CancellationResourceCleanupOutcome,
        CancellationSourceClosureObservationRecord,
        CancellationSourceClosureObservationRecordError, CancellationSourceClosureObservationRow,
        CancellationSourceClosureOutcome, CoordinatorClosureManifest,
        CoordinatorClosureManifestError, CoordinatorClosureManifestValidationError,
        CopyCollisionPolicy, CopyEndpoint, CopyIntentArgs, CopySourceExpectation,
        CreateFolderIntentArgs, DurableReadinessWakeObservation, EvidenceBatchId,
        ExternalCaptureBoundary, ExternalNativeString, ExternalRawEventKind,
        ExternalRawNativeAccounting, ExternalRawObservation, ExternalRawObservationEnvelope,
        ExternalRawObservationLimits, ExternalRawObservationMetadata,
        ExternalRawObservationProvenance, ExternalRawObservedPath, ExternalRawPathRole,
        ExternalReconciliationIntentArgs, ExtractAndTrimIntentArgs, FiniteF32Bits,
        FullCapacityAllocation, FullCapacityAllocationClass, FullCapacityPlan,
        FullProgressIntentObservation, HistoryAction, HistoryDirection,
        HistoryExtractedFileObservation, HistoryFolderMove, HistoryFolderRelocation,
        HistoryRecoveryBackupObservation, HistoryRecoveryLeafObservation,
        HistoryRestoreTargetExpectation, HistoryScopeId, HistoryUndoRedoIntentArgs,
        HistoryWaveformRestore, MAX_READINESS_CONTRACT_VERSION_BYTES, NamespaceCommandArgs,
        NamespaceDirectoryEndpoint, NamespaceDirectoryExpectation, NamespaceDirectoryLocator,
        NamespaceEntryEndpoint, NamespaceEntryExpectation, NamespaceFileExpectation,
        NamespaceLeafName, NamespaceMoveIntentArgs, NamespaceRenameIntentArgs, NamespaceTarget,
        ParticipantCheckpoint, ParticipantCheckpointState, RawWavExtractionRecipe,
        ReadinessDeferralBatchObservation, ReadinessSchedulingObservationRecord,
        ReadinessSchedulingObservationRow, ReadinessTargetSetSummary, RetainedDeleteId,
        RetainedFolderObservation, RetainedPurgeDecision, RetainedPurgeIntentArgs,
        RetainedRecoveryRootKind, RetainedResolutionCommandArgs, RetainedResolutionStage,
        RetainedRestoreIntentArgs, RetryErrorClass, RetryLeaseId, SelectionExtractIntentArgs,
        SelectionOutputCommandArgs, SelectionOutputRecipe, SourceClosureManifest,
        SourceClosureManifestError, SourceClosureManifestValidationError, StoredRelativePath,
        UniversalCommandArgs, WaveformEditFrameRange, WaveformFileRevisionObservation,
        WaveformOutputSpec, WaveformSampleFormat, WaveformSourceSpec,
        fingerprint_selected_entry_json,
    };
    use crate::sample_sources::{SourceId, SourcePlaybackType};
    use serde_json::json;
    use uuid::Uuid;

    fn participant_id(value: u128) -> ParticipantId {
        ParticipantId::from_uuid(Uuid::from_u128(value))
    }

    fn valid_topology(
        with_optional_artifact: bool,
    ) -> (ParticipantSet, ParticipantId, ParticipantId, SourceId) {
        let source = participant_id(1);
        let filesystem = participant_id(2);
        let global = participant_id(3);
        let harvest = participant_id(4);
        let projection = participant_id(5);
        let readiness = participant_id(6);
        let artifact = participant_id(7);
        let source_id = SourceId::from_string("source-a");
        let fence = StoredSourceFence::new(source_id.clone(), "root-a", "db-a", 1, 7, 1)
            .expect("source fence");
        let mut descriptors = vec![
            ParticipantDescriptor::new(
                source,
                ParticipantKind::SourceDatabase,
                ParticipantRequirement::Required,
                Some(fence.clone()),
                None,
            )
            .expect("source descriptor"),
            ParticipantDescriptor::new(
                filesystem,
                ParticipantKind::Filesystem,
                ParticipantRequirement::Required,
                None,
                None,
            )
            .expect("filesystem descriptor"),
            ParticipantDescriptor::new(
                global,
                ParticipantKind::GlobalLibrary,
                ParticipantRequirement::Required,
                None,
                None,
            )
            .expect("global descriptor"),
            ParticipantDescriptor::new(
                harvest,
                ParticipantKind::Harvest,
                ParticipantRequirement::Required,
                None,
                None,
            )
            .expect("harvest descriptor"),
            ParticipantDescriptor::new(
                projection,
                ParticipantKind::Projection,
                ParticipantRequirement::Required,
                Some(fence.clone()),
                None,
            )
            .expect("projection descriptor"),
            ParticipantDescriptor::new(
                readiness,
                ParticipantKind::Readiness,
                ParticipantRequirement::Required,
                Some(fence.clone()),
                None,
            )
            .expect("readiness descriptor"),
        ];
        if with_optional_artifact {
            descriptors.push(
                ParticipantDescriptor::new(
                    artifact,
                    ParticipantKind::Artifact,
                    ParticipantRequirement::OptionalRebuildable,
                    Some(fence),
                    Some("waveform".to_owned()),
                )
                .expect("artifact descriptor"),
            );
        }
        let checkpoints = descriptors
            .iter()
            .map(|descriptor| {
                ParticipantCheckpoint::new(
                    descriptor.id(),
                    ParticipantCheckpointState::Pending,
                    None,
                    None,
                )
                .expect("pending checkpoint")
            })
            .collect();
        (
            ParticipantSet::new(descriptors, checkpoints).expect("participant set"),
            source,
            filesystem,
            source_id,
        )
    }

    fn copy_args_at(
        source: ParticipantId,
        source_id: SourceId,
        destination_path: &str,
    ) -> UniversalCommandArgs {
        let source_endpoint = CopyEndpoint::new(
            source,
            source_id.clone(),
            StoredRelativePath::new("source.wav").expect("source path"),
        )
        .expect("source endpoint");
        let destination_endpoint = CopyEndpoint::new(
            source,
            source_id,
            StoredRelativePath::new(destination_path).expect("destination path"),
        )
        .expect("destination endpoint");
        UniversalCommandArgs::Copy(
            CopyIntentArgs::new(
                source_endpoint,
                destination_endpoint,
                CopySourceExpectation::new(7, "file-id", 12, 13).expect("source expectation"),
                CopyCollisionPolicy::NoReplace,
                None,
            )
            .expect("copy arguments"),
        )
    }

    fn namespace_rename_args(source: ParticipantId, source_id: SourceId) -> UniversalCommandArgs {
        let parent = NamespaceDirectoryEndpoint::new(
            source,
            source_id.clone(),
            NamespaceDirectoryLocator::Root,
            NamespaceDirectoryExpectation::new(7, "root-a", Some(1)).expect("root expectation"),
        )
        .expect("parent endpoint");
        let source_entry = NamespaceEntryEndpoint::new(
            source,
            source_id,
            StoredRelativePath::new("old.wav").expect("source path"),
            NamespaceEntryExpectation::RegularFile(
                NamespaceFileExpectation::new(7, "file-a", 12, Some(2)).expect("file expectation"),
            ),
        )
        .expect("source entry");
        let destination = NamespaceTarget::new(
            parent,
            NamespaceLeafName::new("new.wav").expect("destination leaf"),
        )
        .expect("destination target");
        UniversalCommandArgs::Namespace(NamespaceCommandArgs::Rename(
            NamespaceRenameIntentArgs::new(source_entry, destination).expect("rename arguments"),
        ))
    }

    fn namespace_file_move_args(
        source: ParticipantId,
        source_id: SourceId,
    ) -> UniversalCommandArgs {
        let destination_parent = NamespaceDirectoryEndpoint::new(
            source,
            source_id.clone(),
            NamespaceDirectoryLocator::child(
                StoredRelativePath::new("archive").expect("archive path"),
            )
            .expect("archive locator"),
            NamespaceDirectoryExpectation::new(7, "archive", Some(1)).expect("archive"),
        )
        .expect("destination parent");
        let source_entry = NamespaceEntryEndpoint::new(
            source,
            source_id,
            StoredRelativePath::new("source.wav").expect("source path"),
            NamespaceEntryExpectation::RegularFile(
                NamespaceFileExpectation::new(7, "file-a", 12, Some(2)).expect("file"),
            ),
        )
        .expect("source entry");
        UniversalCommandArgs::Namespace(NamespaceCommandArgs::Move(
            NamespaceMoveIntentArgs::new(source_entry, destination_parent)
                .expect("file move arguments"),
        ))
    }

    fn history_folder_args(source: ParticipantId, source_id: SourceId) -> UniversalCommandArgs {
        let root = NamespaceDirectoryEndpoint::new(
            source,
            source_id.clone(),
            NamespaceDirectoryLocator::Root,
            NamespaceDirectoryExpectation::new(7, "root-a", None).expect("root"),
        )
        .expect("history root");
        let relocation = |source_path: &str, source_identity: &str, destination_path: &str| {
            let source_entry = NamespaceEntryEndpoint::new(
                source,
                source_id.clone(),
                StoredRelativePath::new(source_path).expect("source path"),
                NamespaceEntryExpectation::Directory(
                    NamespaceDirectoryExpectation::new(7, source_identity, None)
                        .expect("source directory"),
                ),
            )
            .expect("source entry");
            let destination = CopyEndpoint::new(
                source,
                source_id.clone(),
                StoredRelativePath::new(destination_path).expect("destination path"),
            )
            .expect("destination");
            HistoryFolderRelocation::new(source_entry, destination).expect("relocation")
        };
        let action = HistoryAction::FolderMove(
            HistoryFolderMove::new(
                root,
                vec![
                    relocation("folder/a", "folder-a", "archive/a"),
                    relocation("folder/b", "folder-b", "archive/b"),
                ],
            )
            .expect("folder move"),
        );
        UniversalCommandArgs::History(
            HistoryUndoRedoIntentArgs::new(
                HistoryScopeId::from_uuid(Uuid::from_u128(310)),
                4,
                5,
                Some(6),
                HistoryDirection::Undo,
                vec![action],
            )
            .expect("history arguments"),
        )
    }

    fn history_restore_with_extracted_args(
        source: ParticipantId,
        filesystem: ParticipantId,
        source_id: SourceId,
    ) -> UniversalCommandArgs {
        let revision = |identity: &str| {
            WaveformFileRevisionObservation::new(identity, "change", 10, Some(11))
                .expect("history revision")
        };
        let target = CopyEndpoint::new(
            source,
            source_id.clone(),
            StoredRelativePath::new("source.wav").expect("target path"),
        )
        .expect("target");
        let extracted_target = CopyEndpoint::new(
            source,
            source_id,
            StoredRelativePath::new("extracted.wav").expect("extracted path"),
        )
        .expect("extracted target");
        let before =
            HistoryRecoveryLeafObservation::new(revision("before")).expect("before backup");
        let extracted_backup = HistoryRecoveryLeafObservation::new(revision("extracted-backup"))
            .expect("extracted backup");
        let backups = HistoryRecoveryBackupObservation::new(
            filesystem,
            "recovery-root",
            Uuid::from_u128(311),
            "edit-directory",
            Some(before),
            None,
            Some(extracted_backup),
        )
        .expect("history backups");
        let extracted = HistoryExtractedFileObservation::undo_remove_expected(
            extracted_target,
            revision("current"),
        )
        .expect("extracted removal");
        let restore = HistoryWaveformRestore::new(
            target,
            7,
            HistoryRestoreTargetExpectation::Existing(revision("target")),
            Some(String::from("before-content")),
            backups,
            Some(extracted),
        )
        .expect("history restore");
        UniversalCommandArgs::History(
            HistoryUndoRedoIntentArgs::new(
                HistoryScopeId::from_uuid(Uuid::from_u128(312)),
                4,
                5,
                Some(6),
                HistoryDirection::Undo,
                vec![HistoryAction::WaveformRestore(restore)],
            )
            .expect("history arguments"),
        )
    }

    fn copy_progress(with_optional_artifact: bool) -> WorkProgress {
        let (participants, source, _filesystem, source_id) = valid_topology(with_optional_artifact);
        let args = copy_args_at(source, source_id, "copy.wav");
        let plan = super::super::work_plan::derive_work_plan(&args, &participants)
            .expect("copy work plan");
        WorkProgress::from_plan(
            OperationId::from_uuid(Uuid::from_u128(if with_optional_artifact {
                101
            } else {
                100
            })),
            &plan,
            &args,
            &participants,
        )
        .expect("copy progress")
    }

    fn copy_plan_inputs() -> (ParticipantSet, UniversalCommandArgs, WorkPlan) {
        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let args = copy_args_at(source, source_id, "copy.wav");
        let plan = super::super::work_plan::derive_work_plan(&args, &participants)
            .expect("copy work plan");
        (participants, args, plan)
    }

    fn copy_snapshot_intent(progress: &WorkProgress) -> FullProgressIntentObservation {
        copy_snapshot_intent_with_capacity(progress, admission_capacity_plan())
    }

    fn copy_snapshot_intent_with_capacity(
        progress: &WorkProgress,
        capacity_plan: FullCapacityPlan,
    ) -> FullProgressIntentObservation {
        let (participants, args, plan) = copy_plan_inputs();
        assert_eq!(progress.plan(), &plan);
        FullProgressIntentObservation::new(
            progress.operation_id(),
            args,
            participants,
            plan,
            capacity_plan,
            progress
                .admission_claims()
                .expect("admission claims")
                .clone(),
        )
        .expect("full-progress snapshot intent")
    }

    fn snapshot_intent_for(
        progress: &WorkProgress,
        args: UniversalCommandArgs,
        participants: ParticipantSet,
    ) -> FullProgressIntentObservation {
        FullProgressIntentObservation::new(
            progress.operation_id(),
            args,
            participants,
            progress.plan().clone(),
            admission_capacity_plan(),
            progress
                .admission_claims()
                .expect("admission claims")
                .clone(),
        )
        .expect("full-progress snapshot intent")
    }

    fn copy_progress_with_initial_artifact_fence_split(
        source_revision_delta: u64,
        lease_epoch_delta: u64,
    ) -> (WorkProgress, FullProgressIntentObservation) {
        let (base, source, _filesystem, source_id) = valid_topology(true);
        let descriptors = base
            .descriptors()
            .iter()
            .map(|descriptor| {
                let fence = if descriptor.kind() == ParticipantKind::Artifact {
                    let old = descriptor.source_fence().expect("artifact fence");
                    Some(
                        StoredSourceFence::new(
                            old.source_id().clone(),
                            old.root_identity(),
                            old.database_identity(),
                            old.lifecycle_generation(),
                            old.source_revision() + source_revision_delta,
                            old.lease_epoch() + lease_epoch_delta,
                        )
                        .expect("split artifact fence"),
                    )
                } else {
                    descriptor.source_fence().cloned()
                };
                ParticipantDescriptor::new(
                    descriptor.id(),
                    descriptor.kind(),
                    descriptor.requirement(),
                    fence,
                    descriptor.artifact_key().map(str::to_owned),
                )
                .expect("split descriptor")
            })
            .collect();
        let participants = ParticipantSet::new(descriptors, base.checkpoints().to_vec())
            .expect("initial artifact split participant set");
        let args = copy_args_at(source, source_id, "copy.wav");
        let plan = super::super::work_plan::derive_work_plan(&args, &participants)
            .expect("initial artifact split work plan");
        let progress = WorkProgress::from_plan_with_admission_observations(
            OperationId::from_uuid(Uuid::from_u128(2301 + lease_epoch_delta as u128)),
            &plan,
            &args,
            &participants,
            &admission_capacity_plan(),
            admission_claims(12),
        )
        .expect("initial artifact split progress");
        let intent = FullProgressIntentObservation::new(
            progress.operation_id(),
            args,
            participants,
            plan,
            admission_capacity_plan(),
            progress.admission_claims().expect("claims").clone(),
        )
        .expect("initial artifact split intent");
        (progress, intent)
    }

    fn admission_capacity_plan() -> FullCapacityPlan {
        FullCapacityPlan::new(vec![
            FullCapacityAllocation::new(
                VolumeIdentity { device: 1 },
                1,
                FullCapacityAllocationClass::DestinationStaging,
                12,
                12,
                PROTECTED_FREE_SPACE_FLOOR,
            )
            .expect("full capacity allocation"),
        ])
        .expect("full capacity plan")
    }

    fn admission_claims(allocated_bytes: u64) -> AdmissionClaimInventory {
        AdmissionClaimInventory::new(vec![
            AdmissionClaim::new(
                AdmissionClaimId::from_uuid(Uuid::from_u128(201)).expect("claim id"),
                VolumeIdentity { device: 1 },
                FullCapacityAllocationClass::DestinationStaging,
                allocated_bytes,
            )
            .expect("claim"),
        ])
        .expect("claim inventory")
    }

    fn copy_progress_with_claim_class(
        allocation_class: FullCapacityAllocationClass,
        operation_value: u128,
    ) -> WorkProgress {
        let (participants, args, plan) = copy_plan_inputs();
        let capacity_plan = FullCapacityPlan::new(vec![
            FullCapacityAllocation::new(
                VolumeIdentity { device: 1 },
                1,
                allocation_class,
                12,
                12,
                PROTECTED_FREE_SPACE_FLOOR,
            )
            .expect("class capacity allocation"),
        ])
        .expect("class capacity plan");
        let claims = AdmissionClaimInventory::new(vec![
            AdmissionClaim::new(
                AdmissionClaimId::from_uuid(Uuid::from_u128(201)).expect("class claim id"),
                VolumeIdentity { device: 1 },
                allocation_class,
                12,
            )
            .expect("class claim"),
        ])
        .expect("class claim inventory");
        WorkProgress::from_plan_with_admission_observations(
            OperationId::from_uuid(Uuid::from_u128(operation_value)),
            &plan,
            &args,
            &participants,
            &capacity_plan,
            claims,
        )
        .expect("class progress")
    }

    fn two_claim_capacity_plan() -> FullCapacityPlan {
        FullCapacityPlan::new(vec![
            FullCapacityAllocation::new(
                VolumeIdentity { device: 1 },
                1,
                FullCapacityAllocationClass::DestinationStaging,
                12,
                12,
                PROTECTED_FREE_SPACE_FLOOR,
            )
            .expect("destination allocation"),
            FullCapacityAllocation::new(
                VolumeIdentity { device: 2 },
                1,
                FullCapacityAllocationClass::OperationRecoveryPayload,
                8,
                8,
                PROTECTED_FREE_SPACE_FLOOR,
            )
            .expect("recovery allocation"),
        ])
        .expect("two-row capacity plan")
    }

    fn two_admission_claims() -> AdmissionClaimInventory {
        AdmissionClaimInventory::new(vec![
            AdmissionClaim::new(
                AdmissionClaimId::from_uuid(Uuid::from_u128(201)).expect("first claim id"),
                VolumeIdentity { device: 1 },
                FullCapacityAllocationClass::DestinationStaging,
                12,
            )
            .expect("first claim"),
            AdmissionClaim::new(
                AdmissionClaimId::from_uuid(Uuid::from_u128(202)).expect("second claim id"),
                VolumeIdentity { device: 2 },
                FullCapacityAllocationClass::OperationRecoveryPayload,
                8,
            )
            .expect("second claim"),
        ])
        .expect("two claim inventory")
    }

    fn copy_progress_with_admission_claims() -> WorkProgress {
        let (participants, args, plan) = copy_plan_inputs();
        WorkProgress::from_plan_with_admission_observations(
            OperationId::from_uuid(Uuid::from_u128(205)),
            &plan,
            &args,
            &participants,
            &admission_capacity_plan(),
            admission_claims(12),
        )
        .expect("copy progress with claim observation")
    }

    fn copy_progress_with_admission_claims_and_optional() -> WorkProgress {
        let (participants, source, _filesystem, source_id) = valid_topology(true);
        let args = copy_args_at(source, source_id, "copy.wav");
        let plan = super::super::work_plan::derive_work_plan(&args, &participants)
            .expect("copy work plan");
        WorkProgress::from_plan_with_admission_observations(
            OperationId::from_uuid(Uuid::from_u128(206)),
            &plan,
            &args,
            &participants,
            &admission_capacity_plan(),
            admission_claims(12),
        )
        .expect("copy progress with optional claim observation")
    }

    fn cross_source_optional_progress() -> WorkProgress {
        let source_a = participant_id(301);
        let source_b = participant_id(302);
        let filesystem = participant_id(303);
        let global = participant_id(304);
        let harvest = participant_id(305);
        let projection_a = participant_id(306);
        let readiness_a = participant_id(307);
        let projection_b = participant_id(308);
        let readiness_b = participant_id(309);
        let artifact_a = participant_id(310);
        let artifact_b = participant_id(311);
        let source_a_id = SourceId::from_string("source-a");
        let source_b_id = SourceId::from_string("source-b");
        let fence_a = StoredSourceFence::new(source_a_id.clone(), "root-a", "db-a", 1, 7, 1)
            .expect("source A fence");
        let fence_b = StoredSourceFence::new(source_b_id.clone(), "root-b", "db-b", 1, 8, 1)
            .expect("source B fence");
        let descriptors = vec![
            ParticipantDescriptor::new(
                source_a,
                ParticipantKind::SourceDatabase,
                ParticipantRequirement::Required,
                Some(fence_a.clone()),
                None,
            )
            .expect("source A"),
            ParticipantDescriptor::new(
                source_b,
                ParticipantKind::SourceDatabase,
                ParticipantRequirement::Required,
                Some(fence_b.clone()),
                None,
            )
            .expect("source B"),
            ParticipantDescriptor::new(
                filesystem,
                ParticipantKind::Filesystem,
                ParticipantRequirement::Required,
                None,
                None,
            )
            .expect("filesystem"),
            ParticipantDescriptor::new(
                global,
                ParticipantKind::GlobalLibrary,
                ParticipantRequirement::Required,
                None,
                None,
            )
            .expect("global"),
            ParticipantDescriptor::new(
                harvest,
                ParticipantKind::Harvest,
                ParticipantRequirement::Required,
                None,
                None,
            )
            .expect("harvest"),
            ParticipantDescriptor::new(
                projection_a,
                ParticipantKind::Projection,
                ParticipantRequirement::Required,
                Some(fence_a.clone()),
                None,
            )
            .expect("projection A"),
            ParticipantDescriptor::new(
                readiness_a,
                ParticipantKind::Readiness,
                ParticipantRequirement::Required,
                Some(fence_a.clone()),
                None,
            )
            .expect("readiness A"),
            ParticipantDescriptor::new(
                projection_b,
                ParticipantKind::Projection,
                ParticipantRequirement::Required,
                Some(fence_b.clone()),
                None,
            )
            .expect("projection B"),
            ParticipantDescriptor::new(
                readiness_b,
                ParticipantKind::Readiness,
                ParticipantRequirement::Required,
                Some(fence_b.clone()),
                None,
            )
            .expect("readiness B"),
            ParticipantDescriptor::new(
                artifact_a,
                ParticipantKind::Artifact,
                ParticipantRequirement::OptionalRebuildable,
                Some(fence_a),
                Some("waveform-a".to_owned()),
            )
            .expect("Artifact A"),
            ParticipantDescriptor::new(
                artifact_b,
                ParticipantKind::Artifact,
                ParticipantRequirement::OptionalRebuildable,
                Some(fence_b),
                Some("waveform-b".to_owned()),
            )
            .expect("Artifact B"),
        ];
        let checkpoints = descriptors
            .iter()
            .map(|descriptor| {
                ParticipantCheckpoint::new(
                    descriptor.id(),
                    ParticipantCheckpointState::Pending,
                    None,
                    None,
                )
                .expect("checkpoint")
            })
            .collect();
        let participants = ParticipantSet::new(descriptors, checkpoints).expect("participants");
        let source = NamespaceEntryEndpoint::new(
            source_a,
            source_a_id,
            StoredRelativePath::new("source.wav").expect("source path"),
            NamespaceEntryExpectation::RegularFile(
                NamespaceFileExpectation::new(7, "source-file", 32, Some(4))
                    .expect("source expectation"),
            ),
        )
        .expect("source endpoint");
        let destination = NamespaceDirectoryEndpoint::new(
            source_b,
            source_b_id,
            NamespaceDirectoryLocator::Root,
            NamespaceDirectoryExpectation::new(8, "root-b", None).expect("destination parent"),
        )
        .expect("destination endpoint");
        let args = UniversalCommandArgs::Namespace(NamespaceCommandArgs::Move(
            NamespaceMoveIntentArgs::new(source, destination).expect("cross-source move"),
        ));
        let plan = super::super::work_plan::derive_work_plan(&args, &participants)
            .expect("cross-source plan");
        WorkProgress::from_plan(
            OperationId::from_uuid(Uuid::from_u128(312)),
            &plan,
            &args,
            &participants,
        )
        .expect("cross-source progress")
    }

    fn copy_progress_with_two_admission_claims() -> WorkProgress {
        let (participants, args, plan) = copy_plan_inputs();
        WorkProgress::from_plan_with_admission_observations(
            OperationId::from_uuid(Uuid::from_u128(208)),
            &plan,
            &args,
            &participants,
            &two_claim_capacity_plan(),
            two_admission_claims(),
        )
        .expect("copy progress with two claim observations")
    }

    fn settlement_inventory(
        progress: &WorkProgress,
        outcome: AdmissionClaimSettlementOutcome,
    ) -> AdmissionClaimSettlementInventory {
        let claims = progress.admission_claims().expect("admission claims");
        AdmissionClaimSettlementInventory::new(
            claims
                .claims()
                .iter()
                .map(|claim| {
                    AdmissionClaimSettlement::new(claim.claim_id(), outcome).expect("settlement")
                })
                .collect(),
        )
        .expect("settlement inventory")
    }

    fn settlement_inventory_with_outcomes(
        progress: &WorkProgress,
        outcomes: &[AdmissionClaimSettlementOutcome],
    ) -> AdmissionClaimSettlementInventory {
        let claims = progress.admission_claims().expect("admission claims");
        assert_eq!(claims.claims().len(), outcomes.len());
        AdmissionClaimSettlementInventory::new(
            claims
                .claims()
                .iter()
                .zip(outcomes)
                .map(|(claim, outcome)| {
                    AdmissionClaimSettlement::new(claim.claim_id(), *outcome).expect("settlement")
                })
                .collect(),
        )
        .expect("settlement inventory")
    }

    fn settlement_authority(
        progress: &WorkProgress,
        settlement: AdmissionClaimSettlementInventory,
    ) -> VerifiedClaimSettlement {
        VerifiedClaimSettlement {
            operation_id: progress.operation_id,
            plan: progress.plan.clone(),
            execution_epoch: progress.execution_epoch,
            admission_claims: progress
                .admission_claims
                .as_ref()
                .expect("admission claims")
                .clone(),
            settlement,
        }
    }

    fn all_never_dispatch_record(progress: &WorkProgress) -> CancellationDispatchOutcomeRecord {
        let manifest = progress
            .cancellation_dispatch_manifest()
            .expect("cancellation dispatch manifest");
        let rows = manifest
            .keys()
            .iter()
            .map(|key| {
                CancellationDispatchOutcomeRow::new(*key, CancellationDispatchOutcome::NeverEntered)
            })
            .collect();
        CancellationDispatchOutcomeRecord::new(
            manifest,
            progress.execution_epoch,
            progress
                .recovery_generation
                .checked_add(1)
                .expect("dispatch recovery generation"),
            rows,
        )
        .expect("all-never dispatch record")
    }

    fn cleanup_manifest_for(
        progress: &mut WorkProgress,
        request_value: i64,
    ) -> (
        CancellationDispatchOutcomeRecord,
        CancellationEffectRecoveryInventory,
        CancellationCleanupManifest,
    ) {
        progress
            .request_cancellation(
                CancellationRequestObservation::new(request_value).expect("cancel"),
            )
            .expect("cleanup cancellation");
        let dispatch = all_never_dispatch_record(progress);
        let recovery = progress
            .cancellation_effect_recovery_inventory(&dispatch)
            .expect("cleanup recovery inventory");
        let manifest = progress
            .cancellation_cleanup_manifest(&recovery, &dispatch)
            .expect("cleanup manifest");
        (dispatch, recovery, manifest)
    }

    fn resource_cleanup_manifest_for(
        progress: &mut WorkProgress,
        request_value: i64,
    ) -> (
        CancellationDispatchOutcomeRecord,
        CancellationEffectRecoveryInventory,
        CancellationCleanupManifest,
        CancellationResourceCleanupManifest,
    ) {
        let (dispatch, recovery, cleanup) = cleanup_manifest_for(progress, request_value);
        let resource = progress
            .cancellation_resource_cleanup_manifest(&cleanup, &recovery, &dispatch)
            .expect("resource cleanup manifest");
        (dispatch, recovery, cleanup, resource)
    }

    fn pending_resource_cleanup_rows(
        manifest: &CancellationResourceCleanupManifest,
    ) -> Vec<CancellationResourceCleanupObservationRow> {
        manifest
            .keys()
            .iter()
            .map(|key| {
                CancellationResourceCleanupObservationRow::new(
                    *key,
                    CancellationResourceCleanupOutcome::Pending,
                )
            })
            .collect()
    }

    fn namespace_cleanup_manifest_for(
        progress: &mut WorkProgress,
        request_value: i64,
    ) -> (
        CancellationDispatchOutcomeRecord,
        CancellationEffectRecoveryInventory,
        CancellationCleanupManifest,
        CancellationNamespaceCleanupManifest,
    ) {
        let (dispatch, recovery, cleanup) = cleanup_manifest_for(progress, request_value);
        let namespace = progress
            .cancellation_namespace_cleanup_manifest(&cleanup, &recovery, &dispatch)
            .expect("namespace cleanup manifest");
        (dispatch, recovery, cleanup, namespace)
    }

    fn pending_namespace_cleanup_rows(
        manifest: &CancellationNamespaceCleanupManifest,
    ) -> Vec<CancellationNamespaceCleanupObservationRow> {
        manifest
            .keys()
            .iter()
            .map(|key| {
                CancellationNamespaceCleanupObservationRow::new(
                    *key,
                    CancellationNamespaceCleanupOutcome::Pending,
                )
            })
            .collect()
    }

    fn accept_empty_after_effect_reconciliation(progress: &mut WorkProgress) {
        let dispatch = all_never_dispatch_record(progress);
        let (recovery, source, coordinator) = empty_after_effect_inputs(progress, dispatch);
        progress
            .accept_after_effect_reconciliation(after_effect_authority(
                progress,
                recovery,
                source,
                coordinator,
                progress.recovery_generation(),
            ))
            .expect("accept empty after-effect reconciliation");
    }

    fn refresh_after_effect_headers_after_reacquisition(progress: &mut WorkProgress) {
        let previous = progress
            .after_effect_reconciliation
            .as_ref()
            .expect("accepted after-effect reconciliation")
            .clone();
        let previous_dispatch = previous.recovery_inventory().dispatch_outcome();
        let dispatch = CancellationDispatchOutcomeRecord::new(
            previous_dispatch.manifest().clone(),
            progress.execution_epoch(),
            progress
                .recovery_generation()
                .checked_add(1)
                .expect("after-effect generation"),
            previous_dispatch.rows().to_vec(),
        )
        .expect("header-only dispatch refresh");
        let recovery = progress
            .cancellation_effect_recovery_inventory(&dispatch)
            .expect("header-only recovery refresh");
        let source_manifest = progress
            .cancellation_source_closure_manifest(&recovery, &dispatch)
            .expect("header-only source manifest");
        let source = CancellationSourceClosureObservationRecord::new(
            source_manifest.clone(),
            progress.execution_epoch(),
            previous
                .source_closure_observation()
                .observation_generation()
                .checked_add(1)
                .expect("source observation generation"),
            previous.source_closure_observation().rows().to_vec(),
        )
        .expect("header-only source refresh");
        let coordinator_manifest = progress
            .cancellation_coordinator_closure_manifest(&source, &recovery, &dispatch)
            .expect("header-only coordinator manifest");
        let coordinator = CancellationCoordinatorClosureObservationRecord::new(
            coordinator_manifest,
            progress.execution_epoch(),
            previous
                .coordinator_closure_observation()
                .observation_generation()
                .checked_add(1)
                .expect("coordinator observation generation"),
            previous.coordinator_closure_observation().rows().to_vec(),
        )
        .expect("header-only coordinator refresh");
        progress
            .accept_after_effect_reconciliation(after_effect_authority(
                progress,
                recovery,
                source,
                coordinator,
                progress.recovery_generation(),
            ))
            .expect("accept header-only after-effect refresh");
    }

    fn pending_cleanup_observation_records(
        progress: &mut WorkProgress,
        observation_generation: u64,
    ) -> (
        CancellationResourceCleanupObservationRecord,
        CancellationNamespaceCleanupObservationRecord,
    ) {
        let after_effect = progress
            .after_effect_reconciliation()
            .expect("accepted after-effect reconciliation");
        let recovery = after_effect.recovery_inventory();
        let dispatch = recovery.dispatch_outcome();
        let cleanup = progress
            .cancellation_cleanup_manifest(recovery, dispatch)
            .expect("cleanup manifest");
        let resource_manifest = progress
            .cancellation_resource_cleanup_manifest(&cleanup, recovery, dispatch)
            .expect("resource manifest");
        let namespace_manifest = progress
            .cancellation_namespace_cleanup_manifest(&cleanup, recovery, dispatch)
            .expect("namespace manifest");
        let resource = CancellationResourceCleanupObservationRecord::new(
            resource_manifest.clone(),
            progress.execution_epoch(),
            observation_generation,
            pending_resource_cleanup_rows(&resource_manifest),
        )
        .expect("pending resource observations");
        let namespace = CancellationNamespaceCleanupObservationRecord::new(
            namespace_manifest.clone(),
            progress.execution_epoch(),
            observation_generation,
            pending_namespace_cleanup_rows(&namespace_manifest),
        )
        .expect("pending namespace observations");
        (resource, namespace)
    }

    fn cleanup_observation_authority(
        progress: &WorkProgress,
        resource_cleanup_observation: CancellationResourceCleanupObservationRecord,
        namespace_cleanup_observation: CancellationNamespaceCleanupObservationRecord,
        expected_recovery_generation: u64,
    ) -> VerifiedCancellationCleanupObservations {
        VerifiedCancellationCleanupObservations {
            operation_id: progress.operation_id,
            plan: progress.plan.clone(),
            execution_epoch: progress.execution_epoch(),
            cancellation_inventory: progress
                .cancellation_inventory
                .as_ref()
                .expect("cancellation inventory")
                .clone(),
            expected_recovery_generation,
            after_effect_reconciliation: progress
                .after_effect_reconciliation
                .as_ref()
                .expect("accepted after-effect reconciliation")
                .clone(),
            resource_cleanup_observation,
            namespace_cleanup_observation,
        }
    }

    fn accept_pending_cleanup_observations(
        progress: &mut WorkProgress,
    ) -> (
        CancellationResourceCleanupObservationRecord,
        CancellationNamespaceCleanupObservationRecord,
    ) {
        let (resource, namespace) = pending_cleanup_observation_records(progress, 1);
        let authority = cleanup_observation_authority(
            progress,
            resource.clone(),
            namespace.clone(),
            progress.recovery_generation(),
        );
        progress
            .accept_cancellation_cleanup_observations(authority)
            .expect("accept pending cleanup observations");
        (resource, namespace)
    }

    fn dispatch_outcome_for_authority(
        progress: &WorkProgress,
    ) -> CancellationDispatchOutcomeRecord {
        progress
            .pre_publish_cancellation()
            .and_then(PrePublishCancellationObservation::dispatch_outcome)
            .filter(|stored| stored.observation_epoch() == progress.execution_epoch)
            .cloned()
            .unwrap_or_else(|| all_never_dispatch_record(progress))
    }

    fn pre_publish_cleanup_authority(
        progress: &WorkProgress,
        cleanup: PrePublishCleanupObservation,
        settlement: AdmissionClaimSettlementInventory,
    ) -> VerifiedPrePublishCancellation {
        pre_publish_cleanup_authority_with_quiescence(
            progress,
            PrePublishUnstartedQuiescenceObservation::new(Vec::new())
                .expect("empty quiescence observation"),
            cleanup,
            settlement,
        )
    }

    fn pre_publish_cleanup_authority_with_quiescence(
        progress: &WorkProgress,
        unstarted_quiescence: PrePublishUnstartedQuiescenceObservation,
        cleanup: PrePublishCleanupObservation,
        settlement: AdmissionClaimSettlementInventory,
    ) -> VerifiedPrePublishCancellation {
        VerifiedPrePublishCancellation {
            operation_id: progress.operation_id,
            plan: progress.plan.clone(),
            execution_epoch: progress.execution_epoch,
            cancellation_inventory: progress
                .cancellation_inventory
                .as_ref()
                .expect("cancellation inventory")
                .clone(),
            expected_recovery_generation: progress.recovery_generation,
            cleanup,
            unstarted_quiescence,
            settlement,
            dispatch_outcome: dispatch_outcome_for_authority(progress),
        }
    }

    fn unstarted_quiescence_for_action(
        progress: &WorkProgress,
        action_ordinal: u8,
        result: PrePublishUnstartedQuiescenceResult,
        staging_absence: Option<PrePublishAbsenceObservation>,
        operation_recovery_payload_absence: Option<PrePublishAbsenceObservation>,
    ) -> PrePublishUnstartedQuiescenceObservation {
        let inventory = progress
            .cancellation_inventory
            .as_ref()
            .expect("cancellation inventory");
        let keys: Vec<_> = progress
            .uncheckpointed_first_effect_keys(inventory)
            .into_iter()
            .filter(|key| key.action_ordinal() == action_ordinal)
            .collect();
        assert!(!keys.is_empty(), "expected an eligible unstarted boundary");
        PrePublishUnstartedQuiescenceObservation::new(vec![
            PrePublishUnstartedQuiescenceAction::new(
                action_ordinal,
                keys,
                result,
                staging_absence,
                operation_recovery_payload_absence,
            ),
        ])
        .expect("quiescence observation")
    }

    fn claimed_progress(
        participants: ParticipantSet,
        args: UniversalCommandArgs,
        operation_value: u128,
    ) -> WorkProgress {
        let plan =
            super::super::work_plan::derive_work_plan(&args, &participants).expect("work plan");
        WorkProgress::from_plan_with_admission_observations(
            OperationId::from_uuid(Uuid::from_u128(operation_value)),
            &plan,
            &args,
            &participants,
            &admission_capacity_plan(),
            admission_claims(12),
        )
        .expect("progress with claim observation")
    }

    fn retained_folder_observation(
        source: ParticipantId,
        source_id: SourceId,
        retained_value: u128,
    ) -> RetainedFolderObservation {
        let staged = NamespaceEntryEndpoint::new(
            source,
            source_id,
            StoredRelativePath::new(".wavecrate_delete_staging/drums/kick").expect("staged path"),
            NamespaceEntryExpectation::Directory(
                NamespaceDirectoryExpectation::new(7, "staged-directory", None)
                    .expect("staged expectation"),
            ),
        )
        .expect("staged endpoint");
        RetainedFolderObservation::new(
            StoredRelativePath::new("drums/kick").expect("original path"),
            staged,
            StoredRelativePath::new("drums/kick").expect("staged relative"),
            RetainedDeleteId::new(Uuid::from_u128(retained_value)).expect("retained ID"),
            RetainedResolutionStage::Deleted,
            RetainedRecoveryRootKind::LegacyDeleteStaging,
            fingerprint_selected_entry_json(br#"{"id":"retained"}"#)
                .expect("selected entry fingerprint"),
        )
        .expect("retained observation")
    }

    fn retained_resolution_args(
        source: ParticipantId,
        source_id: SourceId,
        restore: bool,
    ) -> UniversalCommandArgs {
        let observation = retained_folder_observation(source, source_id, 206);
        let command = if restore {
            RetainedResolutionCommandArgs::Restore(
                RetainedRestoreIntentArgs::new(observation).expect("restore arguments"),
            )
        } else {
            RetainedResolutionCommandArgs::Purge(
                RetainedPurgeIntentArgs::new(observation, RetainedPurgeDecision::UserConfirmed)
                    .expect("purge arguments"),
            )
        };
        UniversalCommandArgs::RetainedResolution(command)
    }

    fn external_reconciliation_args(
        source: ParticipantId,
        filesystem: ParticipantId,
        source_id: SourceId,
    ) -> UniversalCommandArgs {
        let provenance = ExternalRawObservationProvenance::new(
            source_id,
            None,
            None,
            1,
            ExternalCaptureBoundary::new(1, None, None).expect("capture boundary"),
        )
        .expect("provenance");
        let path = ExternalRawObservedPath::new(
            ExternalNativeString::unix_bytes(b"observed.wav".to_vec()).expect("path"),
            ExternalRawPathRole::Subject,
            None,
        )
        .expect("observed path");
        let observation = ExternalRawObservation::new(
            ExternalRawEventKind::Modify,
            vec![path],
            ExternalRawObservationMetadata::new(),
            0,
        )
        .expect("raw observation");
        let raw = ExternalRawObservationEnvelope::new(
            provenance,
            ExternalRawObservationLimits::new(64, 4096, 4096).expect("raw limits"),
            ExternalRawNativeAccounting::new(1, 12, 4),
            vec![observation],
        )
        .expect("raw envelope");
        UniversalCommandArgs::ExternalReconciliation(
            ExternalReconciliationIntentArgs::new(
                EvidenceBatchId::from_uuid(Uuid::from_u128(207)),
                source,
                filesystem,
                7,
                raw,
            )
            .expect("external arguments"),
        )
    }

    fn fenced_harvest_progress() -> WorkProgress {
        let (base, source, _filesystem, source_id) = valid_topology(false);
        let source_fence = base
            .descriptors()
            .iter()
            .find(|descriptor| descriptor.kind() == ParticipantKind::SourceDatabase)
            .and_then(ParticipantDescriptor::source_fence)
            .expect("source fence")
            .clone();
        let descriptors = base
            .descriptors()
            .iter()
            .map(|descriptor| {
                let fence = if descriptor.kind() == ParticipantKind::Harvest {
                    Some(source_fence.clone())
                } else {
                    descriptor.source_fence().cloned()
                };
                ParticipantDescriptor::new(
                    descriptor.id(),
                    descriptor.kind(),
                    descriptor.requirement(),
                    fence,
                    descriptor.artifact_key().map(str::to_owned),
                )
                .expect("fenced harvest descriptor")
            })
            .collect();
        let participants = ParticipantSet::new(descriptors, base.checkpoints().to_vec())
            .expect("fenced harvest participants");
        let args = copy_args_at(source, source_id, "copy.wav");
        let plan = super::super::work_plan::derive_work_plan(&args, &participants)
            .expect("fenced harvest work plan");
        WorkProgress::from_plan(
            OperationId::from_uuid(Uuid::from_u128(105)),
            &plan,
            &args,
            &participants,
        )
        .expect("fenced harvest progress")
    }

    fn fenced_harvest_progress_with_admission_claims() -> WorkProgress {
        let (base, source, _filesystem, source_id) = valid_topology(false);
        let source_fence = base
            .descriptors()
            .iter()
            .find(|descriptor| descriptor.kind() == ParticipantKind::SourceDatabase)
            .and_then(ParticipantDescriptor::source_fence)
            .expect("source fence")
            .clone();
        let descriptors = base
            .descriptors()
            .iter()
            .map(|descriptor| {
                let fence = if descriptor.kind() == ParticipantKind::Harvest {
                    Some(source_fence.clone())
                } else {
                    descriptor.source_fence().cloned()
                };
                ParticipantDescriptor::new(
                    descriptor.id(),
                    descriptor.kind(),
                    descriptor.requirement(),
                    fence,
                    descriptor.artifact_key().map(str::to_owned),
                )
                .expect("fenced harvest descriptor")
            })
            .collect();
        let participants = ParticipantSet::new(descriptors, base.checkpoints().to_vec())
            .expect("fenced harvest participants");
        let args = copy_args_at(source, source_id, "copy.wav");
        let plan = super::super::work_plan::derive_work_plan(&args, &participants)
            .expect("fenced harvest work plan");
        WorkProgress::from_plan_with_admission_observations(
            OperationId::from_uuid(Uuid::from_u128(1308)),
            &plan,
            &args,
            &participants,
            &admission_capacity_plan(),
            admission_claims(12),
        )
        .expect("fenced harvest progress with claims")
    }

    fn key_for(progress: &WorkProgress, action: u8, role: WorkUnitRole) -> WorkUnitKey {
        progress
            .steps()
            .iter()
            .find(|step| step.key().action_ordinal() == action && step.key().role() == role)
            .map(WorkProgressStep::key)
            .expect("work-plan role")
    }

    fn global_key(progress: &WorkProgress, role: WorkUnitRole) -> WorkUnitKey {
        progress
            .steps()
            .iter()
            .find(|step| step.key().action_ordinal() == u8::MAX && step.key().role() == role)
            .map(WorkProgressStep::key)
            .expect("global work-plan role")
    }

    fn key_for_step(
        progress: &WorkProgress,
        action: u8,
        step_segment: u16,
        role: WorkUnitRole,
    ) -> WorkUnitKey {
        progress
            .steps()
            .iter()
            .find(|step| {
                step.key().action_ordinal() == action
                    && step.key().step_ordinal() / super::super::work_plan::STEP_HISTORY_STRIDE
                        == step_segment
                    && step.key().role() == role
            })
            .map(WorkProgressStep::key)
            .expect("work-plan segment role")
    }

    fn source_observation(
        progress: &WorkProgress,
        key: WorkUnitKey,
        revision: u64,
    ) -> SourceRevisionObservation {
        let fence = progress
            .scope_for(key)
            .expect("source scope")
            .source_fence()
            .expect("source fence");
        SourceRevisionObservation::new(
            fence.source_id().clone(),
            fence.root_identity(),
            fence.database_identity(),
            fence.lifecycle_generation(),
            fence.lease_epoch(),
            revision,
        )
        .expect("source observation")
    }

    fn readiness_row(
        progress: &WorkProgress,
        key: WorkUnitKey,
        wake_value: u128,
    ) -> ReadinessSchedulingObservationRow {
        let revision = progress
            .scope_for(key)
            .and_then(ParticipantScope::source_fence)
            .expect("readiness fence")
            .source_revision();
        readiness_row_at(progress, key, wake_value, revision)
    }

    fn readiness_row_at(
        progress: &WorkProgress,
        key: WorkUnitKey,
        wake_value: u128,
        revision: u64,
    ) -> ReadinessSchedulingObservationRow {
        let scope = progress.scope_for(key).expect("readiness scope").clone();
        ReadinessSchedulingObservationRow::new(
            key,
            CompletionContext::new(progress.execution_epoch(), scope),
            source_observation(progress, key, revision),
            1,
            1,
            "readiness-test-contract-v1",
            ReadinessTargetSetSummary::new([0x2a; 32], 0).expect("target summary"),
            DurableReadinessWakeObservation::new(Uuid::from_u128(wake_value), 1)
                .expect("wake observation"),
        )
        .expect("readiness row")
    }

    fn readiness_deferral_observation(
        progress: &WorkProgress,
        artifact_keys: &[WorkUnitKey],
        generation: u64,
    ) -> ReadinessSchedulingObservationRecord {
        let manifest = progress
            .readiness_deferral_manifest(artifact_keys)
            .expect("readiness deferral manifest");
        let rows = manifest
            .readiness_keys()
            .iter()
            .map(|key| {
                progress
                    .step_for(*key)
                    .and_then(WorkProgressStep::evidence)
                    .and_then(|evidence| match evidence {
                        StoredStepEvidence::ReadinessScheduled { observation } => {
                            Some(observation.clone())
                        }
                        _ => None,
                    })
                    .expect("completed rich readiness evidence")
            })
            .collect();
        ReadinessSchedulingObservationRecord::new(
            manifest,
            progress.execution_epoch(),
            generation,
            rows,
        )
        .expect("readiness deferral observation")
    }

    fn readiness_deferral_authority(
        progress: &WorkProgress,
        observation: ReadinessSchedulingObservationRecord,
        expected_previous_generation: u64,
    ) -> VerifiedReadinessDeferral {
        VerifiedReadinessDeferral {
            operation_id: progress.operation_id,
            plan: progress.plan.clone(),
            execution_epoch: progress.execution_epoch(),
            expected_previous_generation,
            observation,
        }
    }

    fn apply_readiness_deferral(
        progress: &mut WorkProgress,
        artifact_keys: &[WorkUnitKey],
        generation: u64,
    ) {
        let observation = readiness_deferral_observation(progress, artifact_keys, generation);
        let expected_previous_generation = progress
            .readiness_deferral_batches()
            .last()
            .map_or(0, |batch| batch.observation_generation());
        progress
            .apply_verified_readiness_deferral(readiness_deferral_authority(
                progress,
                observation,
                expected_previous_generation,
            ))
            .expect("readiness deferral applies");
    }

    fn evidence_for(
        progress: &WorkProgress,
        key: WorkUnitKey,
        revision: u64,
    ) -> StoredStepEvidence {
        match key.role() {
            WorkUnitRole::SourceRead => StoredStepEvidence::SourceObserved {
                observation: source_observation(progress, key, revision),
            },
            WorkUnitRole::SourceAffect
            | WorkUnitRole::DestinationSourceReconcile
            | WorkUnitRole::OriginSourceReconcile => StoredStepEvidence::SourceCommitted {
                before_revision: revision,
                after: source_observation(progress, key, revision),
            },
            WorkUnitRole::Readiness => StoredStepEvidence::ReadinessScheduled {
                observation: readiness_row_at(
                    progress,
                    key,
                    key.participant_id().as_uuid().as_u128() | 1,
                    revision,
                ),
            },
            role => {
                if let Some(kind) = expected_filesystem_receipt(role) {
                    StoredStepEvidence::Filesystem {
                        receipt: FilesystemReceiptSummary::new(kind, 1),
                    }
                } else {
                    let receipt = CoordinatorReceiptSummary::new(
                        expected_coordinator_receipt(role).expect("coordinator role"),
                        1,
                        None,
                    );
                    let receipt = if progress
                        .scope_for(key)
                        .and_then(ParticipantScope::source_fence)
                        .is_some()
                    {
                        receipt.with_source_observation(source_observation(progress, key, revision))
                    } else {
                        receipt
                    };
                    StoredStepEvidence::Coordinator { receipt }
                }
            }
        }
    }

    fn generic_coordinator_evidence_for(
        progress: &WorkProgress,
        key: WorkUnitKey,
        revision: u64,
    ) -> StoredStepEvidence {
        let receipt = CoordinatorReceiptSummary::new(
            expected_coordinator_receipt(key.role()).expect("coordinator role"),
            1,
            None,
        );
        let receipt = if progress
            .scope_for(key)
            .and_then(ParticipantScope::source_fence)
            .is_some()
        {
            receipt.with_source_observation(source_observation(progress, key, revision))
        } else {
            receipt
        };
        StoredStepEvidence::Coordinator { receipt }
    }

    fn completion(
        progress: &WorkProgress,
        key: WorkUnitKey,
        evidence: StoredStepEvidence,
    ) -> VerifiedStepCompletion {
        VerifiedStepCompletion {
            operation_id: progress.operation_id,
            plan: progress.plan.clone(),
            key,
            scope: progress.scope_for(key).expect("completion scope").clone(),
            execution_epoch: progress.execution_epoch,
            evidence,
        }
    }

    fn dispatch_rows(
        manifest: &CancellationDispatchManifest,
        observed: Option<(WorkUnitKey, StoredStepEvidence, CompletionContext)>,
    ) -> Vec<CancellationDispatchOutcomeRow> {
        let observed = observed.into_iter().collect::<Vec<_>>();
        dispatch_rows_with_observed(manifest, &observed)
    }

    fn dispatch_rows_with_observed(
        manifest: &CancellationDispatchManifest,
        observed: &[(WorkUnitKey, StoredStepEvidence, CompletionContext)],
    ) -> Vec<CancellationDispatchOutcomeRow> {
        manifest
            .keys()
            .iter()
            .map(|key| {
                let outcome = observed
                    .iter()
                    .find(|(observed_key, _, _)| observed_key == key)
                    .map_or(
                        CancellationDispatchOutcome::EntryUncertain,
                        |(_, evidence, context)| {
                            CancellationDispatchOutcome::ObservedAfterCancellation {
                                evidence: evidence.clone(),
                                context: context.clone(),
                            }
                        },
                    );
                CancellationDispatchOutcomeRow::new(*key, outcome)
            })
            .collect()
    }

    fn dispatch_rows_with_never_entered(
        manifest: &CancellationDispatchManifest,
        observed: &[(WorkUnitKey, StoredStepEvidence, CompletionContext)],
    ) -> Vec<CancellationDispatchOutcomeRow> {
        manifest
            .keys()
            .iter()
            .map(|key| {
                let outcome = observed
                    .iter()
                    .find(|(observed_key, _, _)| observed_key == key)
                    .map_or(
                        CancellationDispatchOutcome::NeverEntered,
                        |(_, evidence, context)| {
                            CancellationDispatchOutcome::ObservedAfterCancellation {
                                evidence: evidence.clone(),
                                context: context.clone(),
                            }
                        },
                    );
                CancellationDispatchOutcomeRow::new(*key, outcome)
            })
            .collect()
    }

    fn recovery_dispatch_record(
        progress: &WorkProgress,
        recovery_generation: u64,
        observed: &[(WorkUnitKey, StoredStepEvidence, CompletionContext)],
    ) -> CancellationDispatchOutcomeRecord {
        let manifest = progress
            .cancellation_dispatch_manifest()
            .expect("cancellation dispatch manifest");
        CancellationDispatchOutcomeRecord::new(
            manifest.clone(),
            progress.execution_epoch(),
            recovery_generation,
            dispatch_rows_with_observed(&manifest, observed),
        )
        .expect("cancellation dispatch record")
    }

    fn source_closure_rows(
        manifest: &SourceClosureManifest,
    ) -> Vec<CancellationSourceClosureObservationRow> {
        manifest
            .keys()
            .iter()
            .map(|key| {
                CancellationSourceClosureObservationRow::new(
                    *key,
                    CancellationSourceClosureOutcome::Pending,
                )
            })
            .collect()
    }

    fn coordinator_closure_rows(
        manifest: &CoordinatorClosureManifest,
    ) -> Vec<CancellationCoordinatorClosureObservationRow> {
        manifest
            .keys()
            .iter()
            .map(|key| {
                CancellationCoordinatorClosureObservationRow::new(
                    *key,
                    CancellationCoordinatorClosureOutcome::Pending,
                )
            })
            .collect()
    }

    fn coordinator_closure_record(
        manifest: CoordinatorClosureManifest,
        observation_epoch: u64,
        observation_generation: u64,
    ) -> CancellationCoordinatorClosureObservationRecord {
        CancellationCoordinatorClosureObservationRecord::new(
            manifest.clone(),
            observation_epoch,
            observation_generation,
            coordinator_closure_rows(&manifest),
        )
        .expect("coordinator closure record")
    }

    fn after_effect_authority(
        progress: &WorkProgress,
        recovery_inventory: CancellationEffectRecoveryInventory,
        source_closure_observation: CancellationSourceClosureObservationRecord,
        coordinator_closure_observation: CancellationCoordinatorClosureObservationRecord,
        expected_recovery_generation: u64,
    ) -> VerifiedAfterEffectReconciliation {
        VerifiedAfterEffectReconciliation {
            operation_id: progress.operation_id,
            plan: progress.plan.clone(),
            execution_epoch: progress.execution_epoch,
            cancellation_inventory: progress
                .cancellation_inventory
                .as_ref()
                .expect("cancellation inventory")
                .clone(),
            expected_recovery_generation,
            recovery_inventory,
            source_closure_observation,
            coordinator_closure_observation,
        }
    }

    fn empty_after_effect_inputs(
        progress: &WorkProgress,
        dispatch: CancellationDispatchOutcomeRecord,
    ) -> (
        CancellationEffectRecoveryInventory,
        CancellationSourceClosureObservationRecord,
        CancellationCoordinatorClosureObservationRecord,
    ) {
        let recovery = progress
            .cancellation_effect_recovery_inventory(&dispatch)
            .expect("recovery inventory");
        let source_manifest = progress
            .cancellation_source_closure_manifest(&recovery, &dispatch)
            .expect("source closure manifest");
        let source = CancellationSourceClosureObservationRecord::new(
            source_manifest.clone(),
            progress.execution_epoch(),
            1,
            source_closure_rows(&source_manifest),
        )
        .expect("source closure observation");
        let coordinator_manifest = progress
            .cancellation_coordinator_closure_manifest(&source, &recovery, &dispatch)
            .expect("coordinator closure manifest");
        let coordinator =
            coordinator_closure_record(coordinator_manifest, progress.execution_epoch(), 1);
        (recovery, source, coordinator)
    }

    fn assert_after_effect_acceptance_preserves_frozen_state(
        mut progress: WorkProgress,
        request_id: i64,
        expected_route: CancellationRoute,
    ) {
        progress
            .request_cancellation(CancellationRequestObservation::new(request_id).expect("cancel"))
            .expect("request cancellation");
        assert_eq!(
            progress
                .cancellation_inventory()
                .expect("cancellation inventory")
                .route(),
            expected_route
        );
        let before_steps = progress.steps().to_vec();
        let before_plan = progress.plan.clone();
        let before_inventory = progress.cancellation_inventory().cloned();
        let before_claims = progress.admission_claims().cloned();
        let before_settlement = progress.claim_settlement().cloned();
        let before_request = progress.cancellation_request();
        let before_phase = progress.phase();
        let before_disposition = progress.disposition();
        let dispatch = all_never_dispatch_record(&progress);
        let (recovery, source, coordinator) = empty_after_effect_inputs(&progress, dispatch);

        progress
            .accept_after_effect_reconciliation(after_effect_authority(
                &progress,
                recovery,
                source,
                coordinator,
                progress.recovery_generation(),
            ))
            .expect("accept nonterminal after-effect state");

        assert_eq!(progress.plan, before_plan);
        assert_eq!(progress.steps(), before_steps.as_slice());
        assert_eq!(progress.cancellation_inventory(), before_inventory.as_ref());
        assert_eq!(progress.admission_claims(), before_claims.as_ref());
        assert_eq!(progress.claim_settlement(), before_settlement.as_ref());
        assert_eq!(progress.cancellation_request(), before_request);
        assert_eq!(progress.phase(), before_phase);
        assert_eq!(progress.disposition(), before_disposition);
        assert_eq!(progress.recovery_generation(), 1);
    }

    fn completed_source_progress() -> (WorkProgress, WorkUnitKey) {
        let mut progress = copy_progress(false);
        let key = key_for(&progress, 0, WorkUnitRole::SourceRead);
        apply_role(&mut progress, 0, WorkUnitRole::SourceRead);
        (progress, key)
    }

    fn source_reacquisition_authority(
        progress: &WorkProgress,
        target_participant_id: ParticipantId,
        revision: u64,
        lease_epoch: u64,
        execution_epoch: u64,
    ) -> VerifiedSourceReacquisition {
        let old_scope = progress
            .scopes
            .iter()
            .find(|scope| scope.participant_id == target_participant_id)
            .expect("source scope")
            .clone();
        let old_fence = old_scope.source_fence().expect("source fence");
        let new_fence = StoredSourceFence::new(
            old_fence.source_id().clone(),
            old_fence.root_identity(),
            old_fence.database_identity(),
            old_fence.lifecycle_generation(),
            revision,
            lease_epoch,
        )
        .expect("new source fence");
        VerifiedSourceReacquisition {
            operation_id: progress.operation_id,
            plan: progress.plan.clone(),
            execution_epoch,
            target_participant_id,
            old_scope,
            new_fence,
        }
    }

    fn source_reacquisition_authority_with_identity(
        progress: &WorkProgress,
        target_participant_id: ParticipantId,
        source_id: SourceId,
        root_identity: &str,
        database_identity: &str,
        lifecycle_generation: u64,
        revision: u64,
        lease_epoch: u64,
        execution_epoch: u64,
    ) -> VerifiedSourceReacquisition {
        let old_scope = progress
            .scopes
            .iter()
            .find(|scope| scope.participant_id == target_participant_id)
            .expect("source scope")
            .clone();
        let new_fence = StoredSourceFence::new(
            source_id,
            root_identity,
            database_identity,
            lifecycle_generation,
            revision,
            lease_epoch,
        )
        .expect("new source fence");
        VerifiedSourceReacquisition {
            operation_id: progress.operation_id,
            plan: progress.plan.clone(),
            execution_epoch,
            target_participant_id,
            old_scope,
            new_fence,
        }
    }

    fn audit_resolution_authority(
        progress: &WorkProgress,
        generation: u64,
        execution_epoch: u64,
    ) -> VerifiedAuditResolution {
        VerifiedAuditResolution {
            operation_id: progress.operation_id,
            plan: progress.plan.clone(),
            execution_epoch,
            generation,
            observation: progress
                .audit_observation()
                .expect("active audit observation")
                .clone(),
        }
    }

    fn apply_role(progress: &mut WorkProgress, action: u8, role: WorkUnitRole) {
        let key = key_for(progress, action, role);
        let evidence = evidence_for(progress, key, 7);
        progress
            .apply_verified_completion(completion(progress, key, evidence))
            .expect("completion applies");
    }

    fn apply_step(progress: &mut WorkProgress, action: u8, step_segment: u16, role: WorkUnitRole) {
        let key = key_for_step(progress, action, step_segment, role);
        let evidence = evidence_for(progress, key, 7);
        progress
            .apply_verified_completion(completion(progress, key, evidence))
            .expect("segmented completion applies");
    }

    fn publish_prefix(progress: &mut WorkProgress) {
        apply_role(progress, 0, WorkUnitRole::SourceRead);
        for role in [
            WorkUnitRole::Prepare,
            WorkUnitRole::Stage,
            WorkUnitRole::CopyValidated,
            WorkUnitRole::Publish,
        ] {
            apply_role(progress, 0, role);
        }
    }

    fn owner_publish_prefix(owner: &mut dispatch_owner::LiveDispatchOwner) {
        let source = key_for(owner.progress(), 0, WorkUnitRole::SourceRead);
        let reserved = owner.issue(source).expect("owner source reservation");
        let entered = owner.enter(reserved).expect("owner source entry");
        owner
            .complete(entered, evidence_for(owner.progress(), source, 7))
            .expect("owner source completion");
        for role in [
            WorkUnitRole::Prepare,
            WorkUnitRole::Stage,
            WorkUnitRole::CopyValidated,
            WorkUnitRole::Publish,
        ] {
            let key = key_for(owner.progress(), 0, role);
            let reserved = owner.issue(key).expect("owner work reservation");
            let entered = owner.enter(reserved).expect("owner work entry");
            owner
                .complete(entered, evidence_for(owner.progress(), key, 7))
                .expect("owner work completion");
        }
    }

    fn owner_pending_cleanup_observation_records(
        owner: &dispatch_owner::LiveDispatchOwner,
        recovery: &CancellationEffectRecoveryInventory,
        dispatch: &CancellationDispatchOutcomeRecord,
        observation_generation: u64,
    ) -> (
        CancellationResourceCleanupObservationRecord,
        CancellationNamespaceCleanupObservationRecord,
    ) {
        let cleanup = owner
            .progress()
            .cancellation_cleanup_manifest(recovery, dispatch)
            .expect("owner cleanup manifest");
        let resource_manifest = owner
            .progress()
            .cancellation_resource_cleanup_manifest(&cleanup, recovery, dispatch)
            .expect("owner resource manifest");
        let namespace_manifest = owner
            .progress()
            .cancellation_namespace_cleanup_manifest(&cleanup, recovery, dispatch)
            .expect("owner namespace manifest");
        let resource = CancellationResourceCleanupObservationRecord::new(
            resource_manifest.clone(),
            owner.progress().execution_epoch(),
            observation_generation,
            pending_resource_cleanup_rows(&resource_manifest),
        )
        .expect("owner resource observations");
        let namespace = CancellationNamespaceCleanupObservationRecord::new(
            namespace_manifest.clone(),
            owner.progress().execution_epoch(),
            observation_generation,
            pending_namespace_cleanup_rows(&namespace_manifest),
        )
        .expect("owner namespace observations");
        (resource, namespace)
    }

    fn complete_required(progress: &mut WorkProgress) {
        complete_required_at(progress, 7);
    }

    fn complete_required_at(progress: &mut WorkProgress, revision: u64) {
        loop {
            let next = progress.steps().iter().find_map(|step| {
                if step.status() != ProgressStepStatus::Pending
                    || step.requirement() != ParticipantRequirement::Required
                {
                    return None;
                }
                let ready = progress
                    .plan
                    .prerequisites()
                    .iter()
                    .filter(|edge| edge.dependent() == step.key())
                    .all(|edge| {
                        matches!(
                            progress
                                .step_for(edge.prerequisite())
                                .map(WorkProgressStep::status),
                            Some(ProgressStepStatus::Completed | ProgressStepStatus::NotApplicable)
                        )
                    });
                ready.then_some(step.key())
            });
            let Some(key) = next else {
                assert!(progress.steps().iter().all(|step| {
                    step.requirement() != ParticipantRequirement::Required
                        || step.status() != ProgressStepStatus::Pending
                }));
                return;
            };
            let evidence = evidence_for(progress, key, revision);
            progress
                .apply_verified_completion(completion(progress, key, evidence))
                .expect("required completion applies");
        }
    }

    #[test]
    fn source_observation_rejects_oversized_text_before_retaining_it() {
        let oversized = "x".repeat(MAX_SOURCE_IDENTITY_BYTES + 1);
        let error = SourceRevisionObservation::new(
            SourceId::from_string("source-a"),
            oversized,
            "database",
            1,
            1,
            7,
        )
        .expect_err("oversized root identity");
        assert!(matches!(
            error,
            ProgressEvidenceError::InvalidSourceObservation(
                ParticipantValidationError::InvalidText { .. }
            )
        ));
    }

    #[test]
    fn initial_states_are_derived_and_wrong_completion_bindings_are_rejected() {
        let mut progress = copy_progress(false);
        assert!(
            progress
                .steps()
                .iter()
                .all(|step| step.status() == ProgressStepStatus::Pending)
        );
        let source_key = key_for(&progress, 0, WorkUnitRole::SourceRead);
        let source_evidence = evidence_for(&progress, source_key, 7);

        let unknown_key = WorkUnitKey::new(
            0,
            999,
            source_key.participant_id(),
            WorkUnitRole::SourceRead,
        );
        let unknown_completion = VerifiedStepCompletion {
            operation_id: progress.operation_id,
            plan: progress.plan.clone(),
            key: unknown_key,
            scope: progress
                .scope_for(source_key)
                .expect("source scope")
                .clone(),
            execution_epoch: progress.execution_epoch,
            evidence: source_evidence.clone(),
        };
        assert!(matches!(
            progress.apply_verified_completion(unknown_completion),
            Err(WorkProgressError::UnknownWorkUnit { key }) if key == unknown_key
        ));

        let filesystem_key = key_for(&progress, 0, WorkUnitRole::Prepare);
        let wrong_role = completion(
            &progress,
            source_key,
            StoredStepEvidence::Filesystem {
                receipt: FilesystemReceiptSummary::new(FilesystemReceiptKind::Prepared, 1),
            },
        );
        assert!(matches!(
            progress.apply_verified_completion(wrong_role),
            Err(WorkProgressError::RoleEvidenceMismatch {
                role: WorkUnitRole::SourceRead
            })
        ));

        let mut wrong_scope = completion(&progress, source_key, source_evidence.clone());
        wrong_scope.scope.participant_id = filesystem_key.participant_id();
        assert!(matches!(
            progress.apply_verified_completion(wrong_scope),
            Err(WorkProgressError::ParticipantScopeMismatch { key }) if key == source_key
        ));

        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let foreign_args = namespace_rename_args(source, source_id);
        let foreign_plan = super::super::work_plan::derive_work_plan(&foreign_args, &participants)
            .expect("foreign plan");
        let foreign_progress = WorkProgress::from_plan(
            OperationId::from_uuid(Uuid::from_u128(102)),
            &foreign_plan,
            &foreign_args,
            &participants,
        )
        .expect("foreign progress");
        let foreign_key = key_for(&foreign_progress, 0, WorkUnitRole::SourceRead);
        let mut foreign_completion = completion(
            &foreign_progress,
            foreign_key,
            evidence_for(&foreign_progress, foreign_key, 7),
        );
        foreign_completion.operation_id = progress.operation_id;
        assert!(matches!(
            progress.apply_verified_completion(foreign_completion),
            Err(WorkProgressError::PlanMismatch)
        ));
    }

    #[test]
    fn completion_context_survives_source_reacquisition_and_replays_historically() {
        let mut progress = copy_progress(false);
        let source_key = key_for(&progress, 0, WorkUnitRole::SourceRead);
        let source_evidence = evidence_for(&progress, source_key, 7);
        let historical_replay = completion(&progress, source_key, source_evidence.clone());
        progress
            .apply_verified_completion(completion(&progress, source_key, source_evidence.clone()))
            .expect("source completion applies");
        let accepted_context = progress
            .step_for(source_key)
            .expect("source step")
            .completion_context()
            .expect("accepted completion context")
            .clone();
        assert_eq!(accepted_context.execution_epoch(), 0);
        assert_eq!(
            accepted_context
                .scope()
                .source_fence()
                .expect("accepted source fence")
                .lease_epoch(),
            1
        );

        let pending_key = key_for(&progress, 0, WorkUnitRole::Prepare);
        let stale_pending = completion(
            &progress,
            pending_key,
            evidence_for(&progress, pending_key, 7),
        );
        let source_participant_id = source_key.participant_id();
        progress
            .reacquire_source(source_reacquisition_authority(
                &progress,
                source_participant_id,
                7,
                2,
                0,
            ))
            .expect("source reacquisition");
        assert_eq!(progress.execution_epoch(), 1);
        assert_eq!(
            progress
                .step_for(source_key)
                .expect("source step")
                .completion_context(),
            Some(&accepted_context)
        );
        progress
            .validate_completion_history()
            .expect("historical completion remains valid");

        let before_replay = progress.clone();
        assert_eq!(
            progress.apply_verified_completion(historical_replay),
            Ok(ProgressApplyOutcome::Replay)
        );
        assert_eq!(progress, before_replay);

        let relabeled = completion(&progress, source_key, source_evidence);
        let before_conflict = progress.clone();
        assert_eq!(
            progress.apply_verified_completion(relabeled),
            Err(WorkProgressError::CompletionReplayConflict { key: source_key })
        );
        assert_eq!(progress, before_conflict);

        assert!(matches!(
            progress.apply_verified_completion(stale_pending),
            Err(WorkProgressError::ExecutionEpochMismatch {
                expected: 1,
                actual: 0
            })
        ));
    }

    #[test]
    fn completion_history_rejects_malformed_context_and_evidence() {
        let source_key = {
            let progress = copy_progress(false);
            key_for(&progress, 0, WorkUnitRole::SourceRead)
        };

        let mut missing_context = copy_progress(false);
        apply_role(&mut missing_context, 0, WorkUnitRole::SourceRead);
        missing_context
            .steps
            .iter_mut()
            .find(|step| step.key == source_key)
            .expect("source step")
            .completion_context = None;
        assert_eq!(
            missing_context.validate_completion_history(),
            Err(WorkProgressError::CompletionContextMissing { key: source_key })
        );

        let mut missing_evidence = copy_progress(false);
        apply_role(&mut missing_evidence, 0, WorkUnitRole::SourceRead);
        missing_evidence
            .steps
            .iter_mut()
            .find(|step| step.key == source_key)
            .expect("source step")
            .evidence = None;
        assert_eq!(
            missing_evidence.validate_completion_history(),
            Err(WorkProgressError::CompletionEvidenceMissing { key: source_key })
        );

        let mut future_context = copy_progress(false);
        apply_role(&mut future_context, 0, WorkUnitRole::SourceRead);
        let current_epoch = future_context.execution_epoch();
        future_context
            .steps
            .iter_mut()
            .find(|step| step.key == source_key)
            .expect("source step")
            .completion_context
            .as_mut()
            .expect("completion context")
            .execution_epoch = current_epoch + 1;
        assert_eq!(
            future_context.validate_completion_history(),
            Err(WorkProgressError::CompletionContextEpochFuture {
                key: source_key,
                actual: current_epoch + 1,
                current: current_epoch,
            })
        );
    }

    #[test]
    fn completion_history_rejects_historical_fence_and_evidence_shape_regressions() {
        let (mut future_lease, source_key) = completed_source_progress();
        future_lease.execution_epoch = 1;
        let context = future_lease
            .steps
            .iter_mut()
            .find(|step| step.key == source_key)
            .expect("source step")
            .completion_context
            .as_mut()
            .expect("completion context");
        let current_fence = context.scope.source_fence().expect("source fence").clone();
        context.scope.source_fence = Some(
            StoredSourceFence::new(
                current_fence.source_id().clone(),
                current_fence.root_identity(),
                current_fence.database_identity(),
                current_fence.lifecycle_generation(),
                current_fence.source_revision(),
                current_fence.lease_epoch() + 1,
            )
            .expect("future historical lease"),
        );
        assert_eq!(
            future_lease.validate_completion_history(),
            Err(WorkProgressError::CompletionContextScopeMismatch { key: source_key })
        );

        let (mut same_lease_changed_base, source_key) = completed_source_progress();
        same_lease_changed_base.execution_epoch = 1;
        let context = same_lease_changed_base
            .steps
            .iter_mut()
            .find(|step| step.key == source_key)
            .expect("source step")
            .completion_context
            .as_mut()
            .expect("completion context");
        let current_fence = context.scope.source_fence().expect("source fence").clone();
        context.scope.source_fence = Some(
            StoredSourceFence::new(
                current_fence.source_id().clone(),
                current_fence.root_identity(),
                current_fence.database_identity(),
                current_fence.lifecycle_generation(),
                current_fence.source_revision() - 1,
                current_fence.lease_epoch(),
            )
            .expect("same-lease changed base revision"),
        );
        assert_eq!(
            same_lease_changed_base.validate_completion_history(),
            Err(WorkProgressError::CompletionContextScopeMismatch { key: source_key })
        );

        let (mut future_base, source_key) = completed_source_progress();
        let source_participant_id = source_key.participant_id();
        future_base
            .reacquire_source(source_reacquisition_authority(
                &future_base,
                source_participant_id,
                8,
                2,
                0,
            ))
            .expect("source reacquisition");
        let context = future_base
            .steps
            .iter_mut()
            .find(|step| step.key == source_key)
            .expect("source step")
            .completion_context
            .as_mut()
            .expect("completion context");
        let historical_fence = context
            .scope
            .source_fence()
            .expect("historical source fence")
            .clone();
        context.scope.source_fence = Some(
            StoredSourceFence::new(
                historical_fence.source_id().clone(),
                historical_fence.root_identity(),
                historical_fence.database_identity(),
                historical_fence.lifecycle_generation(),
                9,
                historical_fence.lease_epoch(),
            )
            .expect("future historical base revision"),
        );
        assert_eq!(
            future_base.validate_completion_history(),
            Err(WorkProgressError::CompletionContextScopeMismatch { key: source_key })
        );

        let mut old_observation = copy_progress(false);
        let source_key = key_for(&old_observation, 0, WorkUnitRole::SourceRead);
        let historical_observation = source_observation(&old_observation, source_key, 8);
        old_observation
            .apply_verified_completion(completion(
                &old_observation,
                source_key,
                StoredStepEvidence::SourceObserved {
                    observation: historical_observation,
                },
            ))
            .expect("historical source observation");
        old_observation
            .reacquire_source(source_reacquisition_authority(
                &old_observation,
                source_key.participant_id(),
                8,
                2,
                0,
            ))
            .expect("source reacquisition");
        let current_scope = old_observation
            .scopes
            .iter_mut()
            .find(|scope| scope.participant_id == source_key.participant_id())
            .expect("current source scope");
        let current_fence = current_scope
            .source_fence()
            .expect("current source fence")
            .clone();
        current_scope.source_fence = Some(
            StoredSourceFence::new(
                current_fence.source_id().clone(),
                current_fence.root_identity(),
                current_fence.database_identity(),
                current_fence.lifecycle_generation(),
                7,
                current_fence.lease_epoch(),
            )
            .expect("lowered current baseline"),
        );
        assert!(matches!(
            old_observation.validate_completion_history(),
            Err(WorkProgressError::CompletionHistoryRevisionBeyondCurrentFence {
                key: observed_key,
                observed: 8,
                current_revision: 7,
            }) if observed_key == source_key
        ));

        let (mut malformed_evidence, source_key) = completed_source_progress();
        let evidence = malformed_evidence
            .steps
            .iter_mut()
            .find(|step| step.key == source_key)
            .expect("source step")
            .evidence
            .as_mut()
            .expect("source evidence");
        let StoredStepEvidence::SourceObserved { observation } = evidence else {
            panic!("source read evidence");
        };
        observation.lease_epoch = 0;
        assert!(matches!(
            malformed_evidence.validate_completion_history(),
            Err(WorkProgressError::Evidence(
                ProgressEvidenceError::InvalidSourceObservation(
                    ParticipantValidationError::InvalidLeaseEpoch
                )
            ))
        ));
    }

    #[test]
    fn historical_source_revisions_remain_fenced_after_multiple_reacquisitions() {
        let mut progress = fenced_harvest_progress();
        let source_key = key_for(&progress, 0, WorkUnitRole::SourceRead);
        let source_participant_id = source_key.participant_id();
        apply_role(&mut progress, 0, WorkUnitRole::SourceRead);
        for role in [
            WorkUnitRole::Prepare,
            WorkUnitRole::Stage,
            WorkUnitRole::CopyValidated,
            WorkUnitRole::Publish,
        ] {
            apply_role(&mut progress, 0, role);
        }
        let source_affect = key_for(&progress, 0, WorkUnitRole::SourceAffect);
        progress
            .apply_verified_completion(completion(
                &progress,
                source_affect,
                StoredStepEvidence::SourceCommitted {
                    before_revision: 7,
                    after: source_observation(&progress, source_affect, 9),
                },
            ))
            .expect("historical source revision");
        progress
            .reacquire_source(source_reacquisition_authority(
                &progress,
                source_participant_id,
                9,
                2,
                0,
            ))
            .expect("first source reacquisition");
        apply_role(&mut progress, u8::MAX, WorkUnitRole::GlobalLibrary);
        let harvest_key = key_for(&progress, u8::MAX, WorkUnitRole::Harvest);
        progress
            .apply_verified_completion(completion(
                &progress,
                harvest_key,
                evidence_for(&progress, harvest_key, 9),
            ))
            .expect("historical harvest revision");
        let harvest_context = progress
            .step_for(harvest_key)
            .expect("harvest step")
            .completion_context()
            .expect("harvest context")
            .clone();
        assert_eq!(harvest_context.execution_epoch(), 1);
        assert_eq!(
            harvest_context
                .scope()
                .source_fence()
                .expect("harvest source fence")
                .lease_epoch(),
            2
        );

        progress
            .reacquire_source(source_reacquisition_authority(
                &progress,
                source_participant_id,
                9,
                3,
                1,
            ))
            .expect("second source reacquisition");
        progress
            .validate_completion_history()
            .expect("historical contexts remain valid");
        let projection = key_for(&progress, u8::MAX, WorkUnitRole::Projection);
        progress
            .apply_verified_completion(completion(
                &progress,
                projection,
                evidence_for(&progress, projection, 10),
            ))
            .expect("current projection revision");
        let readiness = key_for(&progress, u8::MAX, WorkUnitRole::Readiness);
        assert!(matches!(
            progress.apply_verified_completion(completion(
                &progress,
                readiness,
                StoredStepEvidence::ReadinessScheduled {
                    observation: readiness_row_at(&progress, readiness, 8574, 9),
                },
            )),
            Err(WorkProgressError::SourceRevisionRegressed {
                role: WorkUnitRole::Readiness,
                observed: 9,
                minimum: 10,
            })
        ));
    }

    #[test]
    fn admission_claim_observation_is_optional_and_read_only() {
        let (participants, args, plan) = copy_plan_inputs();
        let capacity_plan = admission_capacity_plan();
        let claims = admission_claims(12);

        let plain = WorkProgress::from_plan(
            OperationId::from_uuid(Uuid::from_u128(202)),
            &plan,
            &args,
            &participants,
        )
        .expect("ordinary progress");
        assert!(plain.admission_claims().is_none());

        let progress = WorkProgress::from_plan_with_admission_observations(
            OperationId::from_uuid(Uuid::from_u128(203)),
            &plan,
            &args,
            &participants,
            &capacity_plan,
            claims.clone(),
        )
        .expect("progress with claim observation");
        let retained = progress.admission_claims().expect("retained claims");
        assert_eq!(retained, &claims);
        assert_eq!(retained.claims(), claims.claims());
    }

    #[test]
    fn invalid_admission_claim_coverage_is_rejected_before_progress_creation() {
        let (participants, args, plan) = copy_plan_inputs();
        let capacity_plan = admission_capacity_plan();
        let claims = admission_claims(11);

        let error = WorkProgress::from_plan_with_admission_observations(
            OperationId::from_uuid(Uuid::from_u128(204)),
            &plan,
            &args,
            &participants,
            &capacity_plan,
            claims,
        )
        .expect_err("under-covered claims");
        assert!(matches!(
            error,
            WorkProgressError::AdmissionClaims(AdmissionClaimValidationError::UnderAllocated {
                expected_bytes: 12,
                observed_bytes: 11,
                ..
            })
        ));
    }

    #[test]
    fn cancellation_requires_admission_claims_without_changing_unadmitted_progress() {
        let mut progress = copy_progress(false);
        let before = progress.clone();
        assert_eq!(
            progress.request_cancellation(
                CancellationRequestObservation::new(206).expect("cancel observation")
            ),
            Err(WorkProgressError::CancellationAdmissionClaimsMissing)
        );
        assert_eq!(progress, before);
        assert!(progress.cancellation_inventory().is_none());
        assert_eq!(progress.disposition(), FullProgressDisposition::None);
    }

    #[test]
    fn committed_source_evidence_records_a_monotonic_before_after_interval() {
        let mut progress = copy_progress(false);
        apply_role(&mut progress, 0, WorkUnitRole::SourceRead);
        for role in [
            WorkUnitRole::Prepare,
            WorkUnitRole::Stage,
            WorkUnitRole::CopyValidated,
            WorkUnitRole::Publish,
        ] {
            apply_role(&mut progress, 0, role);
        }
        let key = key_for(&progress, 0, WorkUnitRole::SourceAffect);
        let after = source_observation(&progress, key, 8);
        progress
            .apply_verified_completion(completion(
                &progress,
                key,
                StoredStepEvidence::SourceCommitted {
                    before_revision: 7,
                    after,
                },
            ))
            .expect("advanced source commit");

        let mut noop = copy_progress(false);
        apply_role(&mut noop, 0, WorkUnitRole::SourceRead);
        for role in [
            WorkUnitRole::Prepare,
            WorkUnitRole::Stage,
            WorkUnitRole::CopyValidated,
            WorkUnitRole::Publish,
        ] {
            apply_role(&mut noop, 0, role);
        }
        let key = key_for(&noop, 0, WorkUnitRole::SourceAffect);
        let invalid = StoredStepEvidence::SourceCommitted {
            before_revision: 6,
            after: source_observation(&noop, key, 7),
        };
        assert!(matches!(
            noop.apply_verified_completion(completion(&noop, key, invalid)),
            Err(WorkProgressError::SourceRevisionRegressed {
                observed: 6,
                minimum: 7,
                ..
            })
        ));
    }

    #[test]
    fn fenced_coordinator_receipts_follow_the_shared_source_revision_floor() {
        let mut progress = copy_progress(false);
        apply_role(&mut progress, 0, WorkUnitRole::SourceRead);
        for role in [
            WorkUnitRole::Prepare,
            WorkUnitRole::Stage,
            WorkUnitRole::CopyValidated,
            WorkUnitRole::Publish,
        ] {
            apply_role(&mut progress, 0, role);
        }
        let source_affect = key_for(&progress, 0, WorkUnitRole::SourceAffect);
        progress
            .apply_verified_completion(completion(
                &progress,
                source_affect,
                StoredStepEvidence::SourceCommitted {
                    before_revision: 7,
                    after: source_observation(&progress, source_affect, 8),
                },
            ))
            .expect("source commit advances the shared fence floor");

        for role in [WorkUnitRole::GlobalLibrary, WorkUnitRole::Harvest] {
            apply_role(&mut progress, u8::MAX, role);
        }
        let projection = key_for(&progress, u8::MAX, WorkUnitRole::Projection);
        let stale_projection =
            CoordinatorReceiptSummary::new(CoordinatorReceiptKind::Projection, 1, None)
                .with_source_observation(source_observation(&progress, projection, 7));
        assert!(matches!(
            progress.apply_verified_completion(completion(
                &progress,
                projection,
                StoredStepEvidence::Coordinator {
                    receipt: stale_projection,
                },
            )),
            Err(WorkProgressError::SourceRevisionRegressed {
                role: WorkUnitRole::Projection,
                observed: 7,
                minimum: 8,
            })
        ));
        let mismatched_projection =
            CoordinatorReceiptSummary::new(CoordinatorReceiptKind::Projection, 1, None)
                .with_source_observation(
                    SourceRevisionObservation::new(
                        SourceId::from_string("source-a"),
                        "different-root",
                        "db-a",
                        1,
                        1,
                        8,
                    )
                    .expect("mismatched observation shape"),
                );
        assert!(matches!(
            progress.apply_verified_completion(completion(
                &progress,
                projection,
                StoredStepEvidence::Coordinator {
                    receipt: mismatched_projection,
                },
            )),
            Err(WorkProgressError::SourceFenceMismatch {
                role: WorkUnitRole::Projection
            })
        ));
        let current_projection =
            CoordinatorReceiptSummary::new(CoordinatorReceiptKind::Projection, 1, None)
                .with_source_observation(source_observation(&progress, projection, 8));
        progress
            .apply_verified_completion(completion(
                &progress,
                projection,
                StoredStepEvidence::Coordinator {
                    receipt: current_projection,
                },
            ))
            .expect("current projection revision");

        let readiness = key_for(&progress, u8::MAX, WorkUnitRole::Readiness);
        assert!(matches!(
            progress.apply_verified_completion(completion(
                &progress,
                readiness,
                StoredStepEvidence::ReadinessScheduled {
                    observation: readiness_row_at(&progress, readiness, 8796, 7),
                },
            )),
            Err(WorkProgressError::SourceRevisionRegressed {
                role: WorkUnitRole::Readiness,
                observed: 7,
                minimum: 8,
            })
        ));
        progress
            .apply_verified_completion(completion(
                &progress,
                readiness,
                StoredStepEvidence::ReadinessScheduled {
                    observation: readiness_row_at(&progress, readiness, 8813, 8),
                },
            ))
            .expect("current readiness revision");

        let unfenced =
            CoordinatorReceiptSummary::new(CoordinatorReceiptKind::GlobalLibrary, 1, None);
        assert!(unfenced.source_observation().is_none());
    }

    #[test]
    fn normal_readiness_completion_requires_rich_scheduling_evidence_and_snapshot_history() {
        let mut progress = copy_progress_with_admission_claims();
        assert!(
            progress
                .scopes
                .iter()
                .all(|scope| scope.kind() != ParticipantKind::Artifact)
        );
        publish_prefix(&mut progress);
        apply_role(&mut progress, 0, WorkUnitRole::SourceAffect);
        for role in [
            WorkUnitRole::GlobalLibrary,
            WorkUnitRole::Harvest,
            WorkUnitRole::Projection,
        ] {
            apply_role(&mut progress, u8::MAX, role);
        }
        let readiness = key_for(&progress, u8::MAX, WorkUnitRole::Readiness);

        let before_generic = progress.clone();
        assert_eq!(
            progress.apply_verified_completion(completion(
                &progress,
                readiness,
                generic_coordinator_evidence_for(&progress, readiness, 7),
            )),
            Err(WorkProgressError::RoleEvidenceMismatch {
                role: WorkUnitRole::Readiness,
            })
        );
        assert_eq!(progress, before_generic);

        let valid_row = readiness_row_at(&progress, readiness, 8801, 7);
        let wrong_key = WorkUnitKey::new(
            readiness.action_ordinal(),
            readiness.step_ordinal() + 1,
            readiness.participant_id(),
            WorkUnitRole::Readiness,
        );
        let wrong_key_row = ReadinessSchedulingObservationRow::new(
            wrong_key,
            valid_row.context().clone(),
            valid_row.source_observation().clone(),
            valid_row.source_generation(),
            valid_row.readiness_revision(),
            valid_row.contract_version(),
            *valid_row.target_set(),
            valid_row.wake().clone(),
        )
        .expect("wrong-key Readiness row shape");
        let before_wrong_key = progress.clone();
        assert_eq!(
            progress.apply_verified_completion(completion(
                &progress,
                readiness,
                StoredStepEvidence::ReadinessScheduled {
                    observation: wrong_key_row,
                },
            )),
            Err(WorkProgressError::ReadinessEvidenceKeyMismatch {
                expected: readiness,
                actual: wrong_key,
            })
        );
        assert_eq!(progress, before_wrong_key);

        let projection = key_for(&progress, u8::MAX, WorkUnitRole::Projection);
        let wrong_context_row = ReadinessSchedulingObservationRow::new(
            readiness,
            CompletionContext::new(
                progress.execution_epoch(),
                progress
                    .scope_for(projection)
                    .expect("projection scope")
                    .clone(),
            ),
            valid_row.source_observation().clone(),
            valid_row.source_generation(),
            valid_row.readiness_revision(),
            valid_row.contract_version(),
            *valid_row.target_set(),
            DurableReadinessWakeObservation::new(Uuid::from_u128(8802), 1)
                .expect("wrong-context wake"),
        )
        .expect("wrong-context Readiness row shape");
        assert_eq!(
            progress.apply_verified_completion(completion(
                &progress,
                readiness,
                StoredStepEvidence::ReadinessScheduled {
                    observation: wrong_context_row,
                },
            )),
            Err(WorkProgressError::ReadinessEvidenceContextMismatch { key: readiness })
        );

        let stale_row = readiness_row_at(&progress, readiness, 8803, 6);
        assert!(matches!(
            progress.apply_verified_completion(completion(
                &progress,
                readiness,
                StoredStepEvidence::ReadinessScheduled {
                    observation: stale_row,
                },
            )),
            Err(WorkProgressError::SourceRevisionRegressed {
                role: WorkUnitRole::Readiness,
                observed: 6,
                minimum: 7,
            })
        ));

        let wrong_source = SourceRevisionObservation::new(
            SourceId::from_string("other-readiness-source"),
            "root-a",
            "db-a",
            1,
            1,
            7,
        )
        .expect("wrong Readiness source observation");
        let wrong_source_row = ReadinessSchedulingObservationRow::new(
            readiness,
            valid_row.context().clone(),
            wrong_source,
            valid_row.source_generation(),
            valid_row.readiness_revision(),
            valid_row.contract_version(),
            *valid_row.target_set(),
            DurableReadinessWakeObservation::new(Uuid::from_u128(8804), 1)
                .expect("wrong-source wake"),
        )
        .expect("wrong-source Readiness row shape");
        assert_eq!(
            progress.apply_verified_completion(completion(
                &progress,
                readiness,
                StoredStepEvidence::ReadinessScheduled {
                    observation: wrong_source_row,
                },
            )),
            Err(WorkProgressError::SourceFenceMismatch {
                role: WorkUnitRole::Readiness,
            })
        );

        let mut malformed_row_json = serde_json::to_value(&valid_row).expect("row JSON");
        malformed_row_json["contract_version"] = json!(" ");
        let malformed_row: ReadinessSchedulingObservationRow =
            serde_json::from_value(malformed_row_json).expect("malformed row shape");
        let before_malformed = progress.clone();
        assert!(matches!(
            progress.apply_verified_completion(completion(
                &progress,
                readiness,
                StoredStepEvidence::ReadinessScheduled {
                    observation: malformed_row,
                },
            )),
            Err(WorkProgressError::Evidence(
                ProgressEvidenceError::ReadinessSchedulingObservation(error)
            )) if matches!(*error, ReadinessSchedulingObservationError::EmptyContractVersion)
        ));
        assert_eq!(progress, before_malformed);

        let valid_evidence = StoredStepEvidence::ReadinessScheduled {
            observation: valid_row,
        };
        assert_eq!(
            progress
                .apply_verified_completion(
                    completion(&progress, readiness, valid_evidence.clone(),)
                )
                .expect("rich Readiness completion"),
            ProgressApplyOutcome::Applied
        );
        assert_eq!(
            progress
                .apply_verified_completion(completion(&progress, readiness, valid_evidence.clone()))
                .expect("exact Readiness replay"),
            ProgressApplyOutcome::Replay
        );

        let accepted_snapshot = progress
            .capture_full_progress_snapshot(&copy_snapshot_intent(&progress))
            .expect("snapshot with rich Readiness history");
        accepted_snapshot
            .validate_base()
            .expect("rich Readiness snapshot validates");
        let readiness_index = accepted_snapshot
            .steps()
            .iter()
            .position(|step| step.key() == readiness)
            .expect("Readiness snapshot step");
        let mut generic_snapshot_json =
            serde_json::to_value(&accepted_snapshot).expect("snapshot JSON");
        generic_snapshot_json["steps"][readiness_index]["evidence"] =
            serde_json::to_value(generic_coordinator_evidence_for(&progress, readiness, 7))
                .expect("generic Readiness evidence JSON");
        let generic_snapshot: FullProgressSnapshotObservation =
            serde_json::from_value(generic_snapshot_json).expect("generic snapshot shape");
        assert_eq!(
            generic_snapshot.validate_base(),
            Err(FullProgressSnapshotObservationError::Completion {
                source: WorkProgressError::RoleEvidenceMismatch {
                    role: WorkUnitRole::Readiness,
                },
            })
        );

        let source = key_for(&progress, 0, WorkUnitRole::SourceRead);
        progress
            .reacquire_source(source_reacquisition_authority(
                &progress,
                source.participant_id(),
                8,
                2,
                0,
            ))
            .expect("source reacquisition");
        progress
            .validate_completion_history()
            .expect("rich Readiness history survives reacquisition");
        progress
            .capture_full_progress_snapshot(&copy_snapshot_intent(&progress))
            .expect("snapshot with rich Readiness history")
            .validate_base()
            .expect("snapshot validates rich Readiness history");
    }

    #[test]
    fn compound_publication_advances_frontier_before_trim() {
        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let source_endpoint = CopyEndpoint::new(
            source,
            source_id.clone(),
            StoredRelativePath::new("source.wav").expect("source path"),
        )
        .expect("source endpoint");
        let destination_endpoint = CopyEndpoint::new(
            source,
            source_id,
            StoredRelativePath::new("selected.wav").expect("destination path"),
        )
        .expect("destination endpoint");
        let source_spec = WaveformSourceSpec::new(2, 48_000, 16, WaveformSampleFormat::Integer)
            .expect("source spec");
        let extraction = SelectionExtractIntentArgs::new(
            source_endpoint,
            7,
            WaveformFileRevisionObservation::new("file", "change", 256, Some(3))
                .expect("file revision"),
            source_spec,
            10,
            WaveformEditFrameRange::new(2, 8).expect("selection range"),
            SelectionOutputRecipe::RawWav(
                RawWavExtractionRecipe::new(16, FiniteF32Bits::new(1.0).expect("gain"), 1)
                    .expect("raw recipe"),
            ),
            SourcePlaybackType::OneShot,
            destination_endpoint,
            CopyCollisionPolicy::NoReplace,
        )
        .expect("extraction");
        let args =
            UniversalCommandArgs::SelectionOutput(SelectionOutputCommandArgs::ExtractAndTrim(
                ExtractAndTrimIntentArgs::new(extraction, WaveformOutputSpec::float32(2, 48_000))
                    .expect("compound arguments"),
            ));
        let plan =
            super::super::work_plan::derive_work_plan(&args, &participants).expect("compound plan");
        let mut progress = WorkProgress::from_plan_with_admission_observations(
            OperationId::from_uuid(Uuid::from_u128(103)),
            &plan,
            &args,
            &participants,
            &admission_capacity_plan(),
            admission_claims(12),
        )
        .expect("compound progress");
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::Prepare,
            WorkUnitRole::Stage,
            WorkUnitRole::CopyValidated,
            WorkUnitRole::Publish,
        ] {
            apply_role(&mut progress, 0, role);
        }
        assert_eq!(progress.phase(), OperationPhase::FilesystemPublished);
        assert_eq!(
            progress
                .step_for(key_for(&progress, 1, WorkUnitRole::SourceRead))
                .expect("trim source read")
                .status(),
            ProgressStepStatus::Pending
        );
        assert_eq!(progress.disposition(), FullProgressDisposition::None);

        progress
            .request_cancellation(CancellationRequestObservation::new(205).expect("cancel"))
            .expect("compound cancellation");
        let inventory = progress
            .cancellation_inventory()
            .expect("compound cancellation inventory");
        assert_eq!(inventory.route(), CancellationRoute::AfterPublish);
        assert_eq!(
            inventory.actions()[0].state(),
            CancellationActionState::AppPublished
        );
        assert_eq!(
            inventory.actions()[1].state(),
            CancellationActionState::NotStarted
        );
        let effects = progress
            .cancellation_effect_inventory()
            .expect("compound cancellation effects");
        assert_eq!(effects.bundles().len(), 1);
        let publication = &effects.bundles()[0];
        assert_eq!(publication.action_ordinal(), 0);
        assert_eq!(publication.step_segment(), 0);
        assert_eq!(
            publication.recipe(),
            CancellationEffectRecipe::StagedPublish
        );
        assert_eq!(publication.outcome(), CancellationEffectOutcome::Published);
        assert_eq!(publication.facts().len(), 1);
        assert_eq!(
            publication.facts()[0].kind(),
            CancellationEffectFactKind::Publish
        );
    }

    #[test]
    fn cancellation_effect_inventory_keeps_history_relocations_distinct() {
        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let mut progress =
            claimed_progress(participants, history_folder_args(source, source_id), 313);
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::Prepare,
            WorkUnitRole::EnsureContainedParentDirectories,
            WorkUnitRole::QualifiedRelocation,
            WorkUnitRole::OriginAbsence,
            WorkUnitRole::SourceAffect,
        ] {
            apply_step(&mut progress, 0, 0, role);
        }
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::Prepare,
            WorkUnitRole::EnsureContainedParentDirectories,
            WorkUnitRole::QualifiedRelocation,
            WorkUnitRole::OriginAbsence,
        ] {
            apply_step(&mut progress, 0, 1, role);
        }
        progress
            .request_cancellation(CancellationRequestObservation::new(313).expect("cancel"))
            .expect("history cancellation");

        let effects = progress
            .cancellation_effect_inventory()
            .expect("history cancellation effects");
        assert_eq!(effects.bundles().len(), 2);
        for (segment, bundle) in effects.bundles().iter().enumerate() {
            assert_eq!(bundle.action_ordinal(), 0);
            assert_eq!(bundle.step_segment(), segment as u16);
            assert_eq!(
                bundle.recipe(),
                CancellationEffectRecipe::QualifiedRelocation
            );
            assert_eq!(
                bundle.outcome(),
                CancellationEffectOutcome::QualifiedRelocationCompleted
            );
            assert_eq!(
                bundle
                    .facts()
                    .iter()
                    .map(|fact| fact.kind())
                    .collect::<Vec<_>>(),
                vec![
                    CancellationEffectFactKind::QualifiedRelocation,
                    CancellationEffectFactKind::OriginAbsence,
                ]
            );
        }
    }

    #[test]
    fn cancellation_effect_inventory_records_standalone_history_extract_removal() {
        let (participants, source, filesystem, source_id) = valid_topology(false);
        let mut progress = claimed_progress(
            participants,
            history_restore_with_extracted_args(source, filesystem, source_id),
            314,
        );
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::RecoveryMaterialRetained,
            WorkUnitRole::Prepare,
            WorkUnitRole::Stage,
            WorkUnitRole::CopyValidated,
            WorkUnitRole::Publish,
            WorkUnitRole::SourceAffect,
        ] {
            apply_step(&mut progress, 0, 0, role);
        }
        for role in [
            WorkUnitRole::OriginRemovalStart,
            WorkUnitRole::OriginAbsence,
        ] {
            apply_step(&mut progress, 0, 1, role);
        }
        progress
            .request_cancellation(CancellationRequestObservation::new(314).expect("cancel"))
            .expect("history extract cancellation");

        let effects = progress
            .cancellation_effect_inventory()
            .expect("history extract effects");
        assert_eq!(effects.bundles().len(), 2);
        assert_eq!(
            effects.bundles()[0].recipe(),
            CancellationEffectRecipe::StagedPublish
        );
        assert_eq!(effects.bundles()[0].step_segment(), 0);
        let extracted = &effects.bundles()[1];
        assert_eq!(
            extracted.recipe(),
            CancellationEffectRecipe::HistoryUndoExtractedRemoval
        );
        assert_eq!(extracted.step_segment(), 1);
        assert_eq!(
            extracted.outcome(),
            CancellationEffectOutcome::RemovalConfirmed
        );
        assert_eq!(
            extracted
                .facts()
                .iter()
                .map(|fact| fact.kind())
                .collect::<Vec<_>>(),
            vec![
                CancellationEffectFactKind::OriginRemovalStart,
                CancellationEffectFactKind::OriginAbsence,
            ]
        );
        assert!(
            !extracted
                .facts()
                .iter()
                .any(|fact| fact.kind() == CancellationEffectFactKind::Publish)
        );
    }

    #[test]
    fn cancellation_effect_inventory_retains_merge_and_publication_facts() {
        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let mut progress = claimed_progress(
            participants,
            retained_resolution_args(source, source_id, true),
            315,
        );
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::Prepare,
            WorkUnitRole::RetainedMerge,
            WorkUnitRole::Publish,
        ] {
            apply_role(&mut progress, 0, role);
        }
        progress
            .request_cancellation(CancellationRequestObservation::new(315).expect("cancel"))
            .expect("retained merge cancellation");

        let effects = progress
            .cancellation_effect_inventory()
            .expect("retained merge effects");
        assert_eq!(effects.bundles().len(), 1);
        let merge = &effects.bundles()[0];
        assert_eq!(merge.recipe(), CancellationEffectRecipe::RetainedMerge);
        assert_eq!(
            merge.outcome(),
            CancellationEffectOutcome::RetainedMergeCompleted
        );
        assert_eq!(
            merge
                .facts()
                .iter()
                .map(|fact| fact.kind())
                .collect::<Vec<_>>(),
            vec![
                CancellationEffectFactKind::RetainedMerge,
                CancellationEffectFactKind::Publish,
            ]
        );
    }

    #[test]
    fn cancellation_effect_inventory_keeps_staged_move_unresolved_without_origin_absence() {
        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let mut progress = claimed_progress(
            participants,
            namespace_file_move_args(source, source_id),
            316,
        );
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::Prepare,
            WorkUnitRole::Stage,
            WorkUnitRole::CopyValidated,
            WorkUnitRole::Publish,
            WorkUnitRole::DestinationSourceReconcile,
            WorkUnitRole::OriginRemovalStart,
        ] {
            apply_role(&mut progress, 0, role);
        }
        progress
            .request_cancellation(CancellationRequestObservation::new(316).expect("cancel"))
            .expect("staged move cancellation");

        let effects = progress
            .cancellation_effect_inventory()
            .expect("staged move effects");
        assert_eq!(effects.bundles().len(), 1);
        let move_effect = &effects.bundles()[0];
        assert_eq!(move_effect.recipe(), CancellationEffectRecipe::StagedMove);
        assert_eq!(
            move_effect.outcome(),
            CancellationEffectOutcome::RemovalUnresolved
        );
        assert!(
            move_effect
                .facts()
                .iter()
                .any(|fact| fact.kind() == CancellationEffectFactKind::Publish)
        );
        assert!(
            move_effect
                .facts()
                .iter()
                .any(|fact| fact.kind() == CancellationEffectFactKind::OriginRemovalStart)
        );
        assert!(
            !move_effect
                .facts()
                .iter()
                .any(|fact| fact.kind() == CancellationEffectFactKind::OriginAbsence)
        );
        assert_eq!(
            progress
                .step_for(key_for(&progress, 0, WorkUnitRole::OriginAbsence))
                .expect("origin absence")
                .status(),
            ProgressStepStatus::Pending
        );
    }

    #[test]
    fn cancellation_effect_recovery_late_origin_absence_confirms_without_source_reconcile() {
        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let mut progress = claimed_progress(
            participants,
            namespace_file_move_args(source, source_id),
            1307,
        );
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::Prepare,
            WorkUnitRole::Stage,
            WorkUnitRole::CopyValidated,
            WorkUnitRole::Publish,
            WorkUnitRole::DestinationSourceReconcile,
            WorkUnitRole::OriginRemovalStart,
        ] {
            apply_role(&mut progress, 0, role);
        }
        progress
            .request_cancellation(CancellationRequestObservation::new(1307).expect("cancel"))
            .expect("staged move cancellation");
        let late_key = key_for(&progress, 0, WorkUnitRole::OriginAbsence);
        let record = recovery_dispatch_record(
            &progress,
            1,
            &[(
                late_key,
                evidence_for(&progress, late_key, 7),
                CompletionContext::new(
                    progress.execution_epoch(),
                    progress.scope_for(late_key).expect("absence scope").clone(),
                ),
            )],
        );

        let recovery = progress
            .cancellation_effect_recovery_inventory(&record)
            .expect("late origin absence recovery effects");
        let move_effect = recovery
            .bundles()
            .iter()
            .find(|bundle| bundle.recipe() == CancellationEffectRecipe::StagedMove)
            .expect("staged move bundle");
        assert_eq!(
            move_effect.outcome(),
            CancellationEffectOutcome::RemovalConfirmed
        );
        assert!(
            move_effect
                .facts()
                .iter()
                .any(|fact| fact.kind() == CancellationEffectFactKind::OriginAbsence)
        );
        assert_eq!(
            move_effect
                .source_closure()
                .iter()
                .find(|closure| closure.key().role() == WorkUnitRole::OriginSourceReconcile)
                .expect("origin source closure")
                .status(),
            ProgressStepStatus::Pending
        );
        assert_eq!(
            progress
                .step_for(key_for(&progress, 0, WorkUnitRole::OriginSourceReconcile))
                .expect("origin source reconcile")
                .status(),
            ProgressStepStatus::Pending
        );
    }

    #[test]
    fn cancellation_effect_recovery_overlays_late_publish_without_future_trim() {
        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let source_endpoint = CopyEndpoint::new(
            source,
            source_id.clone(),
            StoredRelativePath::new("source.wav").expect("source path"),
        )
        .expect("source endpoint");
        let destination_endpoint = CopyEndpoint::new(
            source,
            source_id,
            StoredRelativePath::new("selected.wav").expect("destination path"),
        )
        .expect("destination endpoint");
        let source_spec = WaveformSourceSpec::new(2, 48_000, 16, WaveformSampleFormat::Integer)
            .expect("source spec");
        let extraction = SelectionExtractIntentArgs::new(
            source_endpoint,
            7,
            WaveformFileRevisionObservation::new("file", "change", 256, Some(3))
                .expect("file revision"),
            source_spec,
            10,
            WaveformEditFrameRange::new(2, 8).expect("selection range"),
            SelectionOutputRecipe::RawWav(
                RawWavExtractionRecipe::new(16, FiniteF32Bits::new(1.0).expect("gain"), 1)
                    .expect("raw recipe"),
            ),
            SourcePlaybackType::OneShot,
            destination_endpoint,
            CopyCollisionPolicy::NoReplace,
        )
        .expect("extraction");
        let args =
            UniversalCommandArgs::SelectionOutput(SelectionOutputCommandArgs::ExtractAndTrim(
                ExtractAndTrimIntentArgs::new(extraction, WaveformOutputSpec::float32(2, 48_000))
                    .expect("compound arguments"),
            ));
        let plan =
            super::super::work_plan::derive_work_plan(&args, &participants).expect("compound plan");
        let mut progress = WorkProgress::from_plan_with_admission_observations(
            OperationId::from_uuid(Uuid::from_u128(1301)),
            &plan,
            &args,
            &participants,
            &admission_capacity_plan(),
            admission_claims(12),
        )
        .expect("compound progress");
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::Prepare,
            WorkUnitRole::Stage,
            WorkUnitRole::CopyValidated,
        ] {
            apply_role(&mut progress, 0, role);
        }
        progress
            .request_cancellation(CancellationRequestObservation::new(1301).expect("cancel"))
            .expect("compound cancellation");
        let publish = key_for(&progress, 0, WorkUnitRole::Publish);
        let context = CompletionContext::new(
            progress.execution_epoch(),
            progress.scope_for(publish).expect("publish scope").clone(),
        );
        let record = recovery_dispatch_record(
            &progress,
            1,
            &[(publish, evidence_for(&progress, publish, 7), context)],
        );
        let frozen = progress
            .cancellation_inventory()
            .expect("frozen cancellation inventory")
            .clone();
        let recovery = progress
            .cancellation_effect_recovery_inventory(&record)
            .expect("recovery inventory");

        assert_eq!(recovery.frozen_inventory(), &frozen);
        assert_eq!(recovery.dispatch_outcome(), &record);
        assert_eq!(recovery.bundles().len(), 1);
        assert_eq!(recovery.bundles()[0].action_ordinal(), 0);
        assert_eq!(
            recovery.bundles()[0].recipe(),
            CancellationEffectRecipe::StagedPublish
        );
        assert_eq!(
            recovery.bundles()[0].outcome(),
            CancellationEffectOutcome::Published
        );
        assert!(
            recovery
                .bundles()
                .iter()
                .all(|bundle| bundle.action_ordinal() != 1)
        );
        assert_eq!(
            progress
                .step_for(key_for(&progress, 1, WorkUnitRole::Publish))
                .expect("future trim publish")
                .status(),
            ProgressStepStatus::Pending
        );
        let cleanup = progress
            .cancellation_cleanup_manifest(&recovery, &record)
            .expect("cleanup manifest retains future trim");
        assert_eq!(cleanup.segments().len(), 2);
        assert_eq!(
            cleanup.segments()[0].recipe(),
            CancellationEffectRecipe::StagedPublish
        );
        assert_eq!(
            cleanup.segments()[1].recipe(),
            CancellationEffectRecipe::StagedPublish
        );
        assert!(
            cleanup.segments()[1]
                .keys()
                .iter()
                .any(|key| key.role() == WorkUnitRole::Publish)
        );
    }

    #[test]
    fn cancellation_effect_recovery_keeps_mixed_history_segments_distinct() {
        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let mut progress =
            claimed_progress(participants, history_folder_args(source, source_id), 1302);
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::Prepare,
            WorkUnitRole::EnsureContainedParentDirectories,
            WorkUnitRole::QualifiedRelocation,
            WorkUnitRole::OriginAbsence,
            WorkUnitRole::SourceAffect,
        ] {
            apply_step(&mut progress, 0, 0, role);
        }
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::Prepare,
            WorkUnitRole::EnsureContainedParentDirectories,
        ] {
            apply_step(&mut progress, 0, 1, role);
        }
        progress
            .request_cancellation(CancellationRequestObservation::new(1302).expect("cancel"))
            .expect("history cancellation");
        let late_key = key_for_step(&progress, 0, 1, WorkUnitRole::QualifiedRelocation);
        let context = CompletionContext::new(
            progress.execution_epoch(),
            progress
                .scope_for(late_key)
                .expect("late relocation scope")
                .clone(),
        );
        let record = recovery_dispatch_record(
            &progress,
            1,
            &[(late_key, evidence_for(&progress, late_key, 7), context)],
        );
        let frozen_effects = progress
            .cancellation_effect_inventory()
            .expect("frozen effects");
        assert_eq!(frozen_effects.bundles().len(), 1);
        let recovery = progress
            .cancellation_effect_recovery_inventory(&record)
            .expect("recovery effects");
        assert_eq!(recovery.bundles().len(), 2);
        assert_eq!(recovery.bundles()[0], frozen_effects.bundles()[0]);
        assert_eq!(recovery.bundles()[1].action_ordinal(), 0);
        assert_eq!(recovery.bundles()[1].step_segment(), 1);
        assert_eq!(
            recovery.bundles()[1].recipe(),
            CancellationEffectRecipe::QualifiedRelocation
        );
        assert_eq!(
            recovery.bundles()[1].outcome(),
            CancellationEffectOutcome::QualifiedRelocationCompleted
        );
        let cleanup = progress
            .cancellation_cleanup_manifest(&recovery, &record)
            .expect("mixed history cleanup manifest");
        assert_eq!(cleanup.segments().len(), 2);
        assert!(cleanup.segments().iter().all(|segment| {
            segment.recipe() == CancellationEffectRecipe::QualifiedRelocation
                && segment
                    .keys()
                    .iter()
                    .any(|key| key.role() == WorkUnitRole::SourceAffect)
        }));
    }

    #[test]
    fn cancellation_effect_recovery_refines_only_matching_source_closure() {
        let (participants, source, filesystem, source_id) = valid_topology(false);
        let mut progress = claimed_progress(
            participants,
            history_restore_with_extracted_args(source, filesystem, source_id),
            1303,
        );
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::RecoveryMaterialRetained,
            WorkUnitRole::Prepare,
            WorkUnitRole::Stage,
            WorkUnitRole::CopyValidated,
            WorkUnitRole::Publish,
            WorkUnitRole::SourceAffect,
        ] {
            apply_step(&mut progress, 0, 0, role);
        }
        for role in [
            WorkUnitRole::OriginRemovalStart,
            WorkUnitRole::OriginAbsence,
        ] {
            apply_step(&mut progress, 0, 1, role);
        }
        progress
            .request_cancellation(CancellationRequestObservation::new(1303).expect("cancel"))
            .expect("history extraction cancellation");
        let late_key = key_for_step(&progress, 0, 1, WorkUnitRole::OriginSourceReconcile);
        let context = CompletionContext::new(
            progress.execution_epoch(),
            progress
                .scope_for(late_key)
                .expect("late source closure scope")
                .clone(),
        );
        let record = recovery_dispatch_record(
            &progress,
            1,
            &[(late_key, evidence_for(&progress, late_key, 7), context)],
        );
        let frozen_effects = progress
            .cancellation_effect_inventory()
            .expect("frozen effects")
            .clone();
        let steps_before_reacquisition = progress.steps().to_vec();
        let frozen_inventory = progress
            .cancellation_inventory()
            .expect("frozen cancellation inventory")
            .clone();
        let before_reacquisition = progress
            .cancellation_effect_recovery_inventory(&record)
            .expect("pre-reacquisition recovery effects");
        let before_cleanup = progress
            .cancellation_cleanup_manifest(&before_reacquisition, &record)
            .expect("pre-reacquisition cleanup manifest");
        let source_participant =
            key_for_step(&progress, 0, 0, WorkUnitRole::SourceRead).participant_id();
        progress
            .reacquire_source(source_reacquisition_authority(
                &progress,
                source_participant,
                7,
                2,
                0,
            ))
            .expect("source reacquisition");
        let after_reacquisition = progress
            .cancellation_effect_recovery_inventory(&record)
            .expect("post-reacquisition recovery effects");
        let after_cleanup = progress
            .cancellation_cleanup_manifest(&after_reacquisition, &record)
            .expect("post-reacquisition cleanup manifest");

        assert_eq!(before_reacquisition, after_reacquisition);
        assert_eq!(before_cleanup, after_cleanup);
        assert_eq!(progress.steps(), steps_before_reacquisition.as_slice());
        assert_eq!(progress.cancellation_inventory(), Some(&frozen_inventory));
        let extracted = &after_reacquisition.bundles()[1];
        assert_eq!(
            extracted.outcome(),
            CancellationEffectOutcome::RemovalConfirmed
        );
        assert_eq!(
            extracted
                .source_closure()
                .iter()
                .find(|closure| closure.key() == late_key)
                .expect("matching source closure")
                .status(),
            ProgressStepStatus::Completed
        );
        assert_eq!(
            after_reacquisition.bundles()[0].source_closure(),
            frozen_effects.bundles()[0].source_closure()
        );
    }

    #[test]
    fn cancellation_source_closure_observation_tracks_staged_move_pending_source() {
        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let mut progress = claimed_progress(
            participants,
            namespace_file_move_args(source, source_id),
            1310,
        );
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::Prepare,
            WorkUnitRole::Stage,
            WorkUnitRole::CopyValidated,
            WorkUnitRole::Publish,
            WorkUnitRole::DestinationSourceReconcile,
            WorkUnitRole::OriginRemovalStart,
        ] {
            apply_role(&mut progress, 0, role);
        }
        progress
            .request_cancellation(CancellationRequestObservation::new(1310).expect("cancel"))
            .expect("staged move cancellation");

        let late_key = key_for(&progress, 0, WorkUnitRole::OriginAbsence);
        let late_context = CompletionContext::new(
            progress.execution_epoch(),
            progress
                .scope_for(late_key)
                .expect("origin absence scope")
                .clone(),
        );
        let dispatch = recovery_dispatch_record(
            &progress,
            1,
            &[(late_key, evidence_for(&progress, late_key, 7), late_context)],
        );
        let recovery = progress
            .cancellation_effect_recovery_inventory(&dispatch)
            .expect("recovery inventory");
        let manifest = progress
            .cancellation_source_closure_manifest(&recovery, &dispatch)
            .expect("source-closure manifest");
        let pending_key = key_for(&progress, 0, WorkUnitRole::OriginSourceReconcile);
        assert_eq!(manifest.keys(), &[pending_key]);
        assert_eq!(
            recovery.bundles()[0]
                .source_closure()
                .iter()
                .find(|closure| closure.key() == pending_key)
                .expect("origin source closure")
                .status(),
            ProgressStepStatus::Pending
        );

        let context = CompletionContext::new(
            progress.execution_epoch(),
            progress
                .scope_for(pending_key)
                .expect("origin source reconcile scope")
                .clone(),
        );
        let observation = CancellationSourceClosureObservationRecord::new(
            manifest.clone(),
            dispatch.observation_epoch(),
            1,
            vec![CancellationSourceClosureObservationRow::new(
                pending_key,
                CancellationSourceClosureOutcome::Observed {
                    evidence: evidence_for(&progress, pending_key, 7),
                    context,
                },
            )],
        )
        .expect("source-closure observation");
        progress
            .validate_cancellation_source_closure_observation_record(
                &observation,
                &recovery,
                &dispatch,
            )
            .expect("source-closure observation validates");

        let missing_rows = CancellationSourceClosureObservationRecord::new(
            manifest.clone(),
            dispatch.observation_epoch(),
            2,
            Vec::new(),
        )
        .expect("missing-row record shape");
        assert_eq!(
            progress.validate_cancellation_source_closure_observation_record(
                &missing_rows,
                &recovery,
                &dispatch,
            ),
            Err(WorkProgressError::CancellationSourceClosureObservation(
                CancellationSourceClosureObservationRecordError::RowCountMismatch {
                    actual: 0,
                    expected: 1,
                }
            ))
        );

        let wrong_scope = CancellationSourceClosureObservationRecord::new(
            manifest.clone(),
            dispatch.observation_epoch(),
            2,
            vec![CancellationSourceClosureObservationRow::new(
                pending_key,
                CancellationSourceClosureOutcome::Observed {
                    evidence: evidence_for(&progress, pending_key, 7),
                    context: CompletionContext::new(
                        progress.execution_epoch(),
                        progress.scope_for(late_key).expect("wrong scope").clone(),
                    ),
                },
            )],
        )
        .expect("wrong-scope record shape");
        assert_eq!(
            progress.validate_cancellation_source_closure_observation_record(
                &wrong_scope,
                &recovery,
                &dispatch,
            ),
            Err(WorkProgressError::CompletionContextScopeMismatch { key: pending_key })
        );

        let wrong_evidence = CancellationSourceClosureObservationRecord::new(
            manifest.clone(),
            dispatch.observation_epoch(),
            2,
            vec![CancellationSourceClosureObservationRow::new(
                pending_key,
                CancellationSourceClosureOutcome::Observed {
                    evidence: StoredStepEvidence::Filesystem {
                        receipt: FilesystemReceiptSummary::new(
                            FilesystemReceiptKind::OriginAbsence,
                            1,
                        ),
                    },
                    context: CompletionContext::new(
                        progress.execution_epoch(),
                        progress
                            .scope_for(pending_key)
                            .expect("source closure scope")
                            .clone(),
                    ),
                },
            )],
        )
        .expect("wrong-evidence record shape");
        assert_eq!(
            progress.validate_cancellation_source_closure_observation_record(
                &wrong_evidence,
                &recovery,
                &dispatch,
            ),
            Err(WorkProgressError::RoleEvidenceMismatch {
                role: WorkUnitRole::OriginSourceReconcile,
            })
        );

        let future_context = CancellationSourceClosureObservationRecord::new(
            manifest.clone(),
            dispatch.observation_epoch(),
            2,
            vec![CancellationSourceClosureObservationRow::new(
                pending_key,
                CancellationSourceClosureOutcome::Observed {
                    evidence: evidence_for(&progress, pending_key, 7),
                    context: CompletionContext::new(
                        progress.execution_epoch() + 1,
                        progress
                            .scope_for(pending_key)
                            .expect("source closure scope")
                            .clone(),
                    ),
                },
            )],
        );
        assert_eq!(
            future_context,
            Err(
                CancellationSourceClosureObservationRecordError::ContextEpochAfterObservation {
                    key: pending_key,
                    context_epoch: progress.execution_epoch() + 1,
                    observation_epoch: dispatch.observation_epoch(),
                }
            )
        );

        assert_eq!(
            progress
                .step_for(pending_key)
                .expect("origin source reconcile")
                .status(),
            ProgressStepStatus::Pending
        );
        assert_eq!(
            recovery.bundles()[0]
                .facts()
                .iter()
                .find(|fact| fact.kind() == CancellationEffectFactKind::OriginAbsence)
                .map(|fact| fact.key()),
            Some(late_key)
        );
    }

    #[test]
    fn cancellation_source_closure_manifest_is_exact_across_history_segments() {
        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let mut progress =
            claimed_progress(participants, history_folder_args(source, source_id), 1311);
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::Prepare,
            WorkUnitRole::EnsureContainedParentDirectories,
            WorkUnitRole::QualifiedRelocation,
            WorkUnitRole::OriginAbsence,
            WorkUnitRole::SourceAffect,
        ] {
            apply_step(&mut progress, 0, 0, role);
        }
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::Prepare,
            WorkUnitRole::EnsureContainedParentDirectories,
        ] {
            apply_step(&mut progress, 0, 1, role);
        }
        progress
            .request_cancellation(CancellationRequestObservation::new(1311).expect("cancel"))
            .expect("history cancellation");
        let late_key = key_for_step(&progress, 0, 1, WorkUnitRole::QualifiedRelocation);
        let dispatch = recovery_dispatch_record(
            &progress,
            1,
            &[(
                late_key,
                evidence_for(&progress, late_key, 7),
                CompletionContext::new(
                    progress.execution_epoch(),
                    progress.scope_for(late_key).expect("late scope").clone(),
                ),
            )],
        );
        let recovery = progress
            .cancellation_effect_recovery_inventory(&dispatch)
            .expect("history recovery inventory");
        let manifest = progress
            .cancellation_source_closure_manifest(&recovery, &dispatch)
            .expect("history source-closure manifest");
        let pending_key = key_for_step(&progress, 0, 1, WorkUnitRole::SourceAffect);
        let completed_key = key_for_step(&progress, 0, 0, WorkUnitRole::SourceAffect);
        assert_eq!(manifest.keys(), &[pending_key]);
        assert!(!manifest.keys().contains(&completed_key));

        let missing = SourceClosureManifest::new(recovery.clone(), Vec::new())
            .expect("missing manifest candidate shape");
        assert_eq!(
            progress.validate_cancellation_source_closure_manifest(&missing, &recovery, &dispatch,),
            Err(WorkProgressError::CancellationSourceClosureManifest(
                SourceClosureManifestError::CandidateMismatch
            ))
        );

        assert_eq!(
            SourceClosureManifest::new(recovery.clone(), vec![pending_key, pending_key]),
            Err(SourceClosureManifestValidationError::KeysNotSortedOrUnique { index: 1 })
        );

        let mut extra_keys = vec![pending_key, completed_key];
        extra_keys.sort();
        let extra = SourceClosureManifest::new(recovery.clone(), extra_keys)
            .expect("extra manifest candidate shape");
        assert_eq!(
            progress.validate_cancellation_source_closure_manifest(&extra, &recovery, &dispatch,),
            Err(WorkProgressError::CancellationSourceClosureManifest(
                SourceClosureManifestError::CandidateMismatch
            ))
        );

        let wrong_row = CancellationSourceClosureObservationRecord::new(
            manifest.clone(),
            dispatch.observation_epoch(),
            1,
            vec![CancellationSourceClosureObservationRow::new(
                completed_key,
                CancellationSourceClosureOutcome::Pending,
            )],
        )
        .expect("wrong-row record shape");
        assert_eq!(
            progress.validate_cancellation_source_closure_observation_record(
                &wrong_row, &recovery, &dispatch,
            ),
            Err(WorkProgressError::CancellationSourceClosureObservation(
                CancellationSourceClosureObservationRecordError::RowKeyMismatch {
                    index: 0,
                    actual: completed_key,
                    expected: pending_key,
                }
            ))
        );
    }

    #[test]
    fn cancellation_source_closure_source_observed_survives_reacquisition_and_epoch_binding() {
        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let mut progress =
            claimed_progress(participants, history_folder_args(source, source_id), 1312);
        progress
            .request_cancellation(CancellationRequestObservation::new(1312).expect("cancel"))
            .expect("history cancellation");
        let late_key = key_for_step(&progress, 0, 0, WorkUnitRole::QualifiedRelocation);
        let dispatch = recovery_dispatch_record(
            &progress,
            1,
            &[(
                late_key,
                evidence_for(&progress, late_key, 7),
                CompletionContext::new(
                    progress.execution_epoch(),
                    progress.scope_for(late_key).expect("late scope").clone(),
                ),
            )],
        );
        let recovery = progress
            .cancellation_effect_recovery_inventory(&dispatch)
            .expect("late relocation recovery inventory");
        let manifest = progress
            .cancellation_source_closure_manifest(&recovery, &dispatch)
            .expect("source-closure manifest");
        let source_read = key_for_step(&progress, 0, 0, WorkUnitRole::SourceRead);
        let source_affect = key_for_step(&progress, 0, 0, WorkUnitRole::SourceAffect);
        assert_eq!(manifest.keys().len(), 2);
        assert!(manifest.keys().contains(&source_read));
        assert!(manifest.keys().contains(&source_affect));

        let source_context = CompletionContext::new(
            progress.execution_epoch(),
            progress
                .scope_for(source_read)
                .expect("source scope")
                .clone(),
        );
        let rows = manifest
            .keys()
            .iter()
            .map(|key| {
                if *key == source_read {
                    CancellationSourceClosureObservationRow::new(
                        *key,
                        CancellationSourceClosureOutcome::Observed {
                            evidence: evidence_for(&progress, *key, 7),
                            context: source_context.clone(),
                        },
                    )
                } else {
                    CancellationSourceClosureObservationRow::new(
                        *key,
                        CancellationSourceClosureOutcome::Pending,
                    )
                }
            })
            .collect();
        let observation = CancellationSourceClosureObservationRecord::new(
            manifest,
            dispatch.observation_epoch(),
            1,
            rows,
        )
        .expect("source observed record");
        progress
            .validate_cancellation_source_closure_observation_record(
                &observation,
                &recovery,
                &dispatch,
            )
            .expect("source observed record validates");

        let steps_before = progress.steps().to_vec();
        let inventory_before = progress
            .cancellation_inventory()
            .expect("frozen cancellation inventory")
            .clone();
        progress
            .reacquire_source(source_reacquisition_authority(
                &progress,
                source_read.participant_id(),
                8,
                2,
                0,
            ))
            .expect("source reacquisition");
        progress
            .validate_cancellation_source_closure_observation_record(
                &observation,
                &recovery,
                &dispatch,
            )
            .expect("historical source-closure record validates");
        assert_eq!(progress.steps(), steps_before.as_slice());
        assert_eq!(progress.cancellation_inventory(), Some(&inventory_before));

        let current_dispatch = recovery_dispatch_record(
            &progress,
            2,
            &[(
                late_key,
                evidence_for(&progress, late_key, 8),
                CompletionContext::new(
                    progress.execution_epoch(),
                    progress
                        .scope_for(late_key)
                        .expect("current late scope")
                        .clone(),
                ),
            )],
        );
        let current_recovery = progress
            .cancellation_effect_recovery_inventory(&current_dispatch)
            .expect("current recovery inventory");
        let current_manifest = progress
            .cancellation_source_closure_manifest(&current_recovery, &current_dispatch)
            .expect("current source-closure manifest");
        let before_dispatch = CancellationSourceClosureObservationRecord::new(
            current_manifest.clone(),
            0,
            2,
            source_closure_rows(&current_manifest),
        );
        assert_eq!(
            before_dispatch,
            Err(
                CancellationSourceClosureObservationRecordError::ObservationEpochBeforeDispatch {
                    actual: 0,
                    dispatch: current_dispatch.observation_epoch(),
                }
            )
        );
        let current_rows = source_closure_rows(&current_manifest);
        let current_observation = CancellationSourceClosureObservationRecord::new(
            current_manifest,
            current_dispatch.observation_epoch(),
            2,
            current_rows,
        )
        .expect("current source-closure record");
        progress
            .validate_cancellation_source_closure_observation_record(
                &current_observation,
                &current_recovery,
                &current_dispatch,
            )
            .expect("current source-closure record validates");
    }

    #[test]
    fn cancellation_coordinator_closure_selects_effect_linked_global_and_source_roles() {
        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let mut progress =
            claimed_progress(participants, history_folder_args(source, source_id), 1313);
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::Prepare,
            WorkUnitRole::EnsureContainedParentDirectories,
            WorkUnitRole::QualifiedRelocation,
            WorkUnitRole::OriginAbsence,
            WorkUnitRole::SourceAffect,
        ] {
            apply_step(&mut progress, 0, 0, role);
        }
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::Prepare,
            WorkUnitRole::EnsureContainedParentDirectories,
        ] {
            apply_step(&mut progress, 0, 1, role);
        }
        progress
            .request_cancellation(CancellationRequestObservation::new(1313).expect("cancel"))
            .expect("history cancellation");
        let late_key = key_for_step(&progress, 0, 1, WorkUnitRole::QualifiedRelocation);
        let dispatch = recovery_dispatch_record(
            &progress,
            1,
            &[(
                late_key,
                evidence_for(&progress, late_key, 7),
                CompletionContext::new(
                    progress.execution_epoch(),
                    progress.scope_for(late_key).expect("late scope").clone(),
                ),
            )],
        );
        let recovery = progress
            .cancellation_effect_recovery_inventory(&dispatch)
            .expect("recovery inventory");
        let source_manifest = progress
            .cancellation_source_closure_manifest(&recovery, &dispatch)
            .expect("source-closure manifest");
        let source_observation = CancellationSourceClosureObservationRecord::new(
            source_manifest.clone(),
            dispatch.observation_epoch(),
            1,
            source_closure_rows(&source_manifest),
        )
        .expect("source-closure observation");
        let manifest = progress
            .cancellation_coordinator_closure_manifest(&source_observation, &recovery, &dispatch)
            .expect("coordinator manifest");

        assert!(
            manifest.keys().iter().all(|key| {
                key.action_ordinal() == super::super::work_plan::GLOBAL_ACTION_ORDINAL
            })
        );
        for role in [
            WorkUnitRole::GlobalLibrary,
            WorkUnitRole::Harvest,
            WorkUnitRole::Projection,
            WorkUnitRole::Readiness,
        ] {
            assert!(manifest.keys().contains(&global_key(&progress, role)));
        }
        let record = coordinator_closure_record(manifest.clone(), dispatch.observation_epoch(), 1);
        progress
            .validate_cancellation_coordinator_closure_observation_record(
                &record,
                &source_observation,
                &recovery,
                &dispatch,
            )
            .expect("pending coordinator observation validates");

        let readiness = global_key(&progress, WorkUnitRole::Readiness);
        let generic_readiness = generic_coordinator_evidence_for(&progress, readiness, 7);
        let generic_readiness_context = CompletionContext::new(
            progress.execution_epoch(),
            progress
                .scope_for(readiness)
                .expect("Readiness coordinator scope")
                .clone(),
        );
        let generic_readiness_record = CancellationCoordinatorClosureObservationRecord::new(
            manifest.clone(),
            dispatch.observation_epoch(),
            1,
            manifest
                .keys()
                .iter()
                .map(|key| {
                    let outcome = if *key == readiness {
                        CancellationCoordinatorClosureOutcome::Observed {
                            evidence: generic_readiness.clone(),
                            context: generic_readiness_context.clone(),
                        }
                    } else {
                        CancellationCoordinatorClosureOutcome::Pending
                    };
                    CancellationCoordinatorClosureObservationRow::new(*key, outcome)
                })
                .collect(),
        )
        .expect("generic Readiness coordinator observation");
        let before_generic_readiness = progress.clone();
        progress
            .validate_cancellation_coordinator_closure_observation_record(
                &generic_readiness_record,
                &source_observation,
                &recovery,
                &dispatch,
            )
            .expect("generic Readiness cancellation receipt remains valid");
        assert_eq!(progress, before_generic_readiness);

        let missing = CoordinatorClosureManifest::new(
            source_observation,
            manifest.keys()[..manifest.keys().len() - 1].to_vec(),
        )
        .expect("missing coordinator manifest shape");
        assert_eq!(
            progress.validate_cancellation_coordinator_closure_manifest(
                &missing,
                record.manifest().source_closure_observation(),
                &recovery,
                &dispatch,
            ),
            Err(WorkProgressError::CancellationCoordinatorClosureManifest(
                CoordinatorClosureManifestError::CandidateMismatch
            ))
        );
    }

    #[test]
    fn cancellation_coordinator_closure_excludes_optional_artifact_and_future_action_keys() {
        let mut progress = copy_progress_with_admission_claims_and_optional();
        publish_prefix(&mut progress);
        progress
            .request_cancellation(CancellationRequestObservation::new(1314).expect("cancel"))
            .expect("copy cancellation");
        let publish = key_for(&progress, 0, WorkUnitRole::Publish);
        let dispatch = recovery_dispatch_record(
            &progress,
            1,
            &[(
                publish,
                evidence_for(&progress, publish, 7),
                CompletionContext::new(
                    progress.execution_epoch(),
                    progress.scope_for(publish).expect("publish scope").clone(),
                ),
            )],
        );
        let recovery = progress
            .cancellation_effect_recovery_inventory(&dispatch)
            .expect("copy recovery inventory");
        let source_manifest = progress
            .cancellation_source_closure_manifest(&recovery, &dispatch)
            .expect("copy source-closure manifest");
        let source_observation = CancellationSourceClosureObservationRecord::new(
            source_manifest.clone(),
            dispatch.observation_epoch(),
            1,
            source_closure_rows(&source_manifest),
        )
        .expect("source-closure observation");
        let manifest = progress
            .cancellation_coordinator_closure_manifest(&source_observation, &recovery, &dispatch)
            .expect("coordinator manifest");
        assert!(manifest.keys().iter().all(|key| {
            key.action_ordinal() == super::super::work_plan::GLOBAL_ACTION_ORDINAL
                && key.role() != WorkUnitRole::Artifact
        }));
    }

    #[test]
    fn cancellation_coordinator_closure_no_effect_is_empty_and_late_source_commit_selects_roles() {
        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let mut preparation = claimed_progress(
            participants.clone(),
            copy_args_at(source, source_id.clone(), "copy.wav"),
            1315,
        );
        apply_role(&mut preparation, 0, WorkUnitRole::SourceRead);
        apply_role(&mut preparation, 0, WorkUnitRole::Prepare);
        preparation
            .request_cancellation(CancellationRequestObservation::new(1315).expect("cancel"))
            .expect("preparation cancellation");
        let preparation_dispatch = recovery_dispatch_record(&preparation, 1, &[]);
        let preparation_recovery = preparation
            .cancellation_effect_recovery_inventory(&preparation_dispatch)
            .expect("preparation recovery");
        let preparation_source_manifest = preparation
            .cancellation_source_closure_manifest(&preparation_recovery, &preparation_dispatch)
            .expect("preparation source manifest");
        assert!(preparation_source_manifest.keys().is_empty());
        let preparation_source_observation = CancellationSourceClosureObservationRecord::new(
            preparation_source_manifest,
            preparation_dispatch.observation_epoch(),
            1,
            Vec::new(),
        )
        .expect("preparation source observation");
        let preparation_coordinator = preparation
            .cancellation_coordinator_closure_manifest(
                &preparation_source_observation,
                &preparation_recovery,
                &preparation_dispatch,
            )
            .expect("preparation coordinator manifest");
        assert!(preparation_coordinator.keys().is_empty());

        let mut late_source =
            claimed_progress(participants, history_folder_args(source, source_id), 1316);
        late_source
            .request_cancellation(CancellationRequestObservation::new(1316).expect("cancel"))
            .expect("late source cancellation");
        let source_key = key_for_step(&late_source, 0, 0, WorkUnitRole::SourceAffect);
        let source_dispatch = recovery_dispatch_record(
            &late_source,
            1,
            &[(
                source_key,
                evidence_for(&late_source, source_key, 8),
                CompletionContext::new(
                    late_source.execution_epoch(),
                    late_source
                        .scope_for(source_key)
                        .expect("source scope")
                        .clone(),
                ),
            )],
        );
        let source_recovery = late_source
            .cancellation_effect_recovery_inventory(&source_dispatch)
            .expect("late source recovery");
        let source_manifest = late_source
            .cancellation_source_closure_manifest(&source_recovery, &source_dispatch)
            .expect("late source manifest");
        let source_observation = CancellationSourceClosureObservationRecord::new(
            source_manifest.clone(),
            source_dispatch.observation_epoch(),
            1,
            source_closure_rows(&source_manifest),
        )
        .expect("late source observation");
        let coordinator = late_source
            .cancellation_coordinator_closure_manifest(
                &source_observation,
                &source_recovery,
                &source_dispatch,
            )
            .expect("late source coordinator manifest");
        assert!(
            coordinator
                .keys()
                .contains(&global_key(&late_source, WorkUnitRole::GlobalLibrary))
        );
        assert!(
            coordinator
                .keys()
                .contains(&global_key(&late_source, WorkUnitRole::Harvest))
        );
        assert!(
            coordinator.keys().iter().all(|key| {
                key.action_ordinal() == super::super::work_plan::GLOBAL_ACTION_ORDINAL
            })
        );
    }

    #[test]
    fn cancellation_coordinator_closure_rejects_stale_linked_source_revision() {
        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let mut progress =
            claimed_progress(participants, history_folder_args(source, source_id), 1317);
        progress
            .request_cancellation(CancellationRequestObservation::new(1317).expect("cancel"))
            .expect("history cancellation");
        let source_key = key_for_step(&progress, 0, 0, WorkUnitRole::SourceAffect);
        let dispatch = recovery_dispatch_record(
            &progress,
            1,
            &[(
                source_key,
                evidence_for(&progress, source_key, 9),
                CompletionContext::new(
                    progress.execution_epoch(),
                    progress
                        .scope_for(source_key)
                        .expect("source scope")
                        .clone(),
                ),
            )],
        );
        let recovery = progress
            .cancellation_effect_recovery_inventory(&dispatch)
            .expect("source recovery");
        let source_manifest = progress
            .cancellation_source_closure_manifest(&recovery, &dispatch)
            .expect("source manifest");
        let source_observation = CancellationSourceClosureObservationRecord::new(
            source_manifest.clone(),
            dispatch.observation_epoch(),
            1,
            source_closure_rows(&source_manifest),
        )
        .expect("source observation");
        let coordinator_manifest = progress
            .cancellation_coordinator_closure_manifest(&source_observation, &recovery, &dispatch)
            .expect("coordinator manifest");
        let coordinator_key = coordinator_manifest
            .keys()
            .iter()
            .copied()
            .find(|key| {
                progress
                    .scope_for(*key)
                    .and_then(ParticipantScope::source_fence)
                    .is_some()
            })
            .expect("source-fenced coordinator");
        let stale_rows = coordinator_manifest
            .keys()
            .iter()
            .map(|key| {
                let outcome = if *key == coordinator_key {
                    CancellationCoordinatorClosureOutcome::Observed {
                        evidence: evidence_for(&progress, *key, 8),
                        context: CompletionContext::new(
                            progress.execution_epoch(),
                            progress.scope_for(*key).expect("coordinator scope").clone(),
                        ),
                    }
                } else {
                    CancellationCoordinatorClosureOutcome::Pending
                };
                CancellationCoordinatorClosureObservationRow::new(*key, outcome)
            })
            .collect();
        let stale = CancellationCoordinatorClosureObservationRecord::new(
            coordinator_manifest.clone(),
            dispatch.observation_epoch(),
            1,
            stale_rows,
        )
        .expect("stale coordinator observation shape");
        assert_eq!(
            progress.validate_cancellation_coordinator_closure_observation_record(
                &stale,
                &source_observation,
                &recovery,
                &dispatch,
            ),
            Err(WorkProgressError::SourceRevisionRegressed {
                role: coordinator_key.role(),
                observed: 8,
                minimum: 9,
            })
        );
        let valid_rows = coordinator_manifest
            .keys()
            .iter()
            .map(|key| {
                let outcome = if *key == coordinator_key {
                    CancellationCoordinatorClosureOutcome::Observed {
                        evidence: evidence_for(&progress, *key, 9),
                        context: CompletionContext::new(
                            progress.execution_epoch(),
                            progress.scope_for(*key).expect("coordinator scope").clone(),
                        ),
                    }
                } else {
                    CancellationCoordinatorClosureOutcome::Pending
                };
                CancellationCoordinatorClosureObservationRow::new(*key, outcome)
            })
            .collect();
        let valid = CancellationCoordinatorClosureObservationRecord::new(
            coordinator_manifest,
            dispatch.observation_epoch(),
            1,
            valid_rows,
        )
        .expect("valid coordinator observation shape");
        progress
            .validate_cancellation_coordinator_closure_observation_record(
                &valid,
                &source_observation,
                &recovery,
                &dispatch,
            )
            .expect("coordinator revision at linked floor validates");
    }

    #[test]
    fn cancellation_coordinator_closure_rejects_bad_receipts_lineage_and_coverage() {
        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let mut progress =
            claimed_progress(participants, history_folder_args(source, source_id), 1318);
        progress
            .request_cancellation(CancellationRequestObservation::new(1318).expect("cancel"))
            .expect("history cancellation");
        let source_key = key_for_step(&progress, 0, 0, WorkUnitRole::SourceAffect);
        let dispatch = recovery_dispatch_record(
            &progress,
            1,
            &[(
                source_key,
                evidence_for(&progress, source_key, 9),
                CompletionContext::new(
                    progress.execution_epoch(),
                    progress
                        .scope_for(source_key)
                        .expect("source scope")
                        .clone(),
                ),
            )],
        );
        let recovery = progress
            .cancellation_effect_recovery_inventory(&dispatch)
            .expect("source recovery");
        let source_manifest = progress
            .cancellation_source_closure_manifest(&recovery, &dispatch)
            .expect("source manifest");
        let source_observation = CancellationSourceClosureObservationRecord::new(
            source_manifest.clone(),
            dispatch.observation_epoch(),
            1,
            source_closure_rows(&source_manifest),
        )
        .expect("source observation");
        let manifest = progress
            .cancellation_coordinator_closure_manifest(&source_observation, &recovery, &dispatch)
            .expect("coordinator manifest");
        let source_coordinator = manifest
            .keys()
            .iter()
            .copied()
            .find(|key| {
                progress
                    .scope_for(*key)
                    .and_then(ParticipantScope::source_fence)
                    .is_some()
            })
            .expect("source-fenced coordinator");
        let global = global_key(&progress, WorkUnitRole::GlobalLibrary);
        let make_record = |target: WorkUnitKey,
                           outcome: CancellationCoordinatorClosureOutcome|
         -> CancellationCoordinatorClosureObservationRecord {
            let rows = manifest
                .keys()
                .iter()
                .map(|key| {
                    if *key == target {
                        CancellationCoordinatorClosureObservationRow::new(*key, outcome.clone())
                    } else {
                        CancellationCoordinatorClosureObservationRow::new(
                            *key,
                            CancellationCoordinatorClosureOutcome::Pending,
                        )
                    }
                })
                .collect();
            CancellationCoordinatorClosureObservationRecord::new(
                manifest.clone(),
                dispatch.observation_epoch(),
                1,
                rows,
            )
            .expect("coordinator record shape")
        };

        let wrong_kind = make_record(
            global,
            CancellationCoordinatorClosureOutcome::Observed {
                evidence: StoredStepEvidence::Coordinator {
                    receipt: CoordinatorReceiptSummary::new(
                        CoordinatorReceiptKind::Harvest,
                        1,
                        None,
                    ),
                },
                context: CompletionContext::new(
                    progress.execution_epoch(),
                    progress.scope_for(global).expect("global scope").clone(),
                ),
            },
        );
        assert_eq!(
            progress.validate_cancellation_coordinator_closure_observation_record(
                &wrong_kind,
                &source_observation,
                &recovery,
                &dispatch,
            ),
            Err(WorkProgressError::RoleEvidenceMismatch {
                role: WorkUnitRole::GlobalLibrary,
            })
        );

        let deficit = make_record(
            global,
            CancellationCoordinatorClosureOutcome::Observed {
                evidence: StoredStepEvidence::Coordinator {
                    receipt: CoordinatorReceiptSummary::new(
                        CoordinatorReceiptKind::GlobalLibrary,
                        1,
                        Some(CoordinatorDeficit::RequiredParticipant(
                            global.participant_id(),
                        )),
                    ),
                },
                context: CompletionContext::new(
                    progress.execution_epoch(),
                    progress.scope_for(global).expect("global scope").clone(),
                ),
            },
        );
        assert_eq!(
            progress.validate_cancellation_coordinator_closure_observation_record(
                &deficit,
                &source_observation,
                &recovery,
                &dispatch,
            ),
            Err(WorkProgressError::CoordinatorDeficitPresent {
                role: WorkUnitRole::GlobalLibrary,
            })
        );

        let missing_rows = manifest
            .keys()
            .iter()
            .map(|key| {
                if *key == source_coordinator {
                    CancellationCoordinatorClosureObservationRow::new(
                        *key,
                        CancellationCoordinatorClosureOutcome::Observed {
                            evidence: StoredStepEvidence::Coordinator {
                                receipt: CoordinatorReceiptSummary::new(
                                    expected_coordinator_receipt(key.role()).expect("receipt kind"),
                                    1,
                                    None,
                                ),
                            },
                            context: CompletionContext::new(
                                progress.execution_epoch(),
                                progress
                                    .scope_for(source_coordinator)
                                    .expect("source coordinator scope")
                                    .clone(),
                            ),
                        },
                    )
                } else {
                    CancellationCoordinatorClosureObservationRow::new(
                        *key,
                        CancellationCoordinatorClosureOutcome::Pending,
                    )
                }
            })
            .collect();
        let missing_receipt = CancellationCoordinatorClosureObservationRecord::new(
            manifest.clone(),
            dispatch.observation_epoch(),
            1,
            missing_rows,
        )
        .expect("missing receipt record shape");
        assert_eq!(
            progress.validate_cancellation_coordinator_closure_observation_record(
                &missing_receipt,
                &source_observation,
                &recovery,
                &dispatch,
            ),
            Err(WorkProgressError::CoordinatorSourceObservationMissing {
                role: source_coordinator.role(),
            })
        );

        let wrong_scope = make_record(
            source_coordinator,
            CancellationCoordinatorClosureOutcome::Observed {
                evidence: evidence_for(&progress, source_coordinator, 9),
                context: CompletionContext::new(
                    progress.execution_epoch(),
                    progress.scope_for(global).expect("wrong scope").clone(),
                ),
            },
        );
        assert_eq!(
            progress.validate_cancellation_coordinator_closure_observation_record(
                &wrong_scope,
                &source_observation,
                &recovery,
                &dispatch,
            ),
            Err(WorkProgressError::CompletionContextScopeMismatch {
                key: source_coordinator,
            })
        );

        let future = CancellationCoordinatorClosureObservationRecord::new(
            manifest.clone(),
            progress.execution_epoch() + 1,
            1,
            coordinator_closure_rows(&manifest),
        )
        .expect("future coordinator record shape");
        assert_eq!(
            progress.validate_cancellation_coordinator_closure_observation_record(
                &future,
                &source_observation,
                &recovery,
                &dispatch,
            ),
            Err(
                WorkProgressError::CancellationCoordinatorClosureObservation(
                    CancellationCoordinatorClosureObservationRecordError::ObservationEpochFuture {
                        actual: 1,
                        current: 0,
                    }
                )
            )
        );

        let duplicate_key = manifest.keys()[0];
        assert_eq!(
            CoordinatorClosureManifest::new(
                source_observation.clone(),
                vec![duplicate_key, duplicate_key],
            ),
            Err(CoordinatorClosureManifestValidationError::KeysNotSortedOrUnique { index: 1 })
        );
        let missing_rows = CancellationCoordinatorClosureObservationRecord::new(
            manifest.clone(),
            dispatch.observation_epoch(),
            1,
            Vec::new(),
        )
        .expect("missing coordinator rows shape");
        assert_eq!(
            progress.validate_cancellation_coordinator_closure_observation_record(
                &missing_rows,
                &source_observation,
                &recovery,
                &dispatch,
            ),
            Err(
                WorkProgressError::CancellationCoordinatorClosureObservation(
                    CancellationCoordinatorClosureObservationRecordError::RowCountMismatch {
                        actual: 0,
                        expected: manifest.keys().len(),
                    }
                )
            )
        );

        let other_source_observation = CancellationSourceClosureObservationRecord::new(
            source_manifest,
            dispatch.observation_epoch(),
            2,
            source_closure_rows(source_observation.manifest()),
        )
        .expect("alternate source observation");
        let mismatched_manifest =
            CoordinatorClosureManifest::new(other_source_observation, manifest.keys().to_vec())
                .expect("mismatched coordinator manifest shape");
        assert_eq!(
            progress.validate_cancellation_coordinator_closure_manifest(
                &mismatched_manifest,
                &source_observation,
                &recovery,
                &dispatch,
            ),
            Err(WorkProgressError::CancellationCoordinatorClosureManifest(
                CoordinatorClosureManifestError::SourceClosureObservationMismatch
            ))
        );
    }

    #[test]
    fn cancellation_coordinator_closure_historical_observation_survives_reacquisition() {
        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let mut progress =
            claimed_progress(participants, history_folder_args(source, source_id), 1319);
        progress
            .request_cancellation(CancellationRequestObservation::new(1319).expect("cancel"))
            .expect("history cancellation");
        let source_key = key_for_step(&progress, 0, 0, WorkUnitRole::SourceAffect);
        let dispatch = recovery_dispatch_record(
            &progress,
            1,
            &[(
                source_key,
                evidence_for(&progress, source_key, 8),
                CompletionContext::new(
                    progress.execution_epoch(),
                    progress
                        .scope_for(source_key)
                        .expect("source scope")
                        .clone(),
                ),
            )],
        );
        let recovery = progress
            .cancellation_effect_recovery_inventory(&dispatch)
            .expect("source recovery");
        let source_manifest = progress
            .cancellation_source_closure_manifest(&recovery, &dispatch)
            .expect("source manifest");
        let source_observation = CancellationSourceClosureObservationRecord::new(
            source_manifest.clone(),
            dispatch.observation_epoch(),
            1,
            source_closure_rows(&source_manifest),
        )
        .expect("source observation");
        let manifest = progress
            .cancellation_coordinator_closure_manifest(&source_observation, &recovery, &dispatch)
            .expect("coordinator manifest");
        let source_coordinator = manifest
            .keys()
            .iter()
            .copied()
            .find(|key| {
                progress
                    .scope_for(*key)
                    .and_then(ParticipantScope::source_fence)
                    .is_some()
            })
            .expect("source-fenced coordinator");
        let rows = manifest
            .keys()
            .iter()
            .map(|key| {
                if *key == source_coordinator {
                    CancellationCoordinatorClosureObservationRow::new(
                        *key,
                        CancellationCoordinatorClosureOutcome::Observed {
                            evidence: evidence_for(&progress, *key, 8),
                            context: CompletionContext::new(
                                progress.execution_epoch(),
                                progress.scope_for(*key).expect("coordinator scope").clone(),
                            ),
                        },
                    )
                } else {
                    CancellationCoordinatorClosureObservationRow::new(
                        *key,
                        CancellationCoordinatorClosureOutcome::Pending,
                    )
                }
            })
            .collect();
        let observation = CancellationCoordinatorClosureObservationRecord::new(
            manifest.clone(),
            dispatch.observation_epoch(),
            1,
            rows,
        )
        .expect("coordinator observation");
        progress
            .validate_cancellation_coordinator_closure_observation_record(
                &observation,
                &source_observation,
                &recovery,
                &dispatch,
            )
            .expect("historical coordinator observation validates");
        let steps_before = progress.steps().to_vec();
        let inventory_before = progress
            .cancellation_inventory()
            .expect("frozen inventory")
            .clone();
        progress
            .reacquire_source(source_reacquisition_authority(
                &progress,
                source_key.participant_id(),
                8,
                2,
                0,
            ))
            .expect("source reacquisition");
        progress
            .validate_cancellation_coordinator_closure_observation_record(
                &observation,
                &source_observation,
                &recovery,
                &dispatch,
            )
            .expect("historical coordinator observation survives reacquisition");
        assert_eq!(progress.steps(), steps_before.as_slice());
        assert_eq!(progress.cancellation_inventory(), Some(&inventory_before));
    }

    #[test]
    fn cancellation_coordinator_closure_floor_includes_linked_source_receipts() {
        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let mut progress = claimed_progress(
            participants,
            namespace_file_move_args(source, source_id),
            1320,
        );
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::Prepare,
            WorkUnitRole::Stage,
            WorkUnitRole::CopyValidated,
            WorkUnitRole::Publish,
        ] {
            apply_role(&mut progress, 0, role);
        }
        let destination_reconcile = key_for(&progress, 0, WorkUnitRole::DestinationSourceReconcile);
        progress
            .apply_verified_completion(completion(
                &progress,
                destination_reconcile,
                evidence_for(&progress, destination_reconcile, 9),
            ))
            .expect("destination reconciliation at revision nine");
        apply_role(&mut progress, 0, WorkUnitRole::OriginRemovalStart);
        progress
            .request_cancellation(CancellationRequestObservation::new(1320).expect("cancel"))
            .expect("move cancellation");
        let late_key = key_for(&progress, 0, WorkUnitRole::OriginAbsence);
        let dispatch = recovery_dispatch_record(
            &progress,
            1,
            &[(
                late_key,
                evidence_for(&progress, late_key, 7),
                CompletionContext::new(
                    progress.execution_epoch(),
                    progress.scope_for(late_key).expect("absence scope").clone(),
                ),
            )],
        );
        let recovery = progress
            .cancellation_effect_recovery_inventory(&dispatch)
            .expect("move recovery");
        let source_manifest = progress
            .cancellation_source_closure_manifest(&recovery, &dispatch)
            .expect("source manifest");
        let pending_source = key_for(&progress, 0, WorkUnitRole::OriginSourceReconcile);
        assert_eq!(source_manifest.keys(), &[pending_source]);
        assert_eq!(
            progress.cancellation_source_revision_floor(
                progress
                    .scope_for(pending_source)
                    .expect("pending source scope")
                    .source_fence()
                    .expect("pending source fence"),
                &dispatch,
                None,
            ),
            Some(9)
        );
        let stale_source_rows = source_manifest
            .keys()
            .iter()
            .map(|key| {
                CancellationSourceClosureObservationRow::new(
                    *key,
                    CancellationSourceClosureOutcome::Observed {
                        evidence: StoredStepEvidence::SourceCommitted {
                            before_revision: 8,
                            after: source_observation(&progress, *key, 10),
                        },
                        context: CompletionContext::new(
                            progress.execution_epoch(),
                            progress
                                .scope_for(*key)
                                .expect("source closure scope")
                                .clone(),
                        ),
                    },
                )
            })
            .collect();
        let stale_source_observation = CancellationSourceClosureObservationRecord::new(
            source_manifest.clone(),
            dispatch.observation_epoch(),
            1,
            stale_source_rows,
        )
        .expect("stale source closure observation shape");
        assert_eq!(
            progress.validate_cancellation_source_closure_observation_record(
                &stale_source_observation,
                &recovery,
                &dispatch,
            ),
            Err(WorkProgressError::SourceRevisionRegressed {
                role: WorkUnitRole::OriginSourceReconcile,
                observed: 8,
                minimum: 9,
            })
        );
        let source_rows = source_manifest
            .keys()
            .iter()
            .map(|key| {
                CancellationSourceClosureObservationRow::new(
                    *key,
                    CancellationSourceClosureOutcome::Observed {
                        evidence: StoredStepEvidence::SourceCommitted {
                            before_revision: 9,
                            after: source_observation(&progress, *key, 10),
                        },
                        context: CompletionContext::new(
                            progress.execution_epoch(),
                            progress
                                .scope_for(*key)
                                .expect("source closure scope")
                                .clone(),
                        ),
                    },
                )
            })
            .collect();
        let source_observation = CancellationSourceClosureObservationRecord::new(
            source_manifest,
            dispatch.observation_epoch(),
            1,
            source_rows,
        )
        .expect("source closure observation");
        progress
            .validate_cancellation_source_closure_observation_record(
                &source_observation,
                &recovery,
                &dispatch,
            )
            .expect("source closure receipt validates");
        let coordinator_manifest = progress
            .cancellation_coordinator_closure_manifest(&source_observation, &recovery, &dispatch)
            .expect("coordinator manifest");
        let coordinator_key = coordinator_manifest
            .keys()
            .iter()
            .copied()
            .find(|key| {
                progress
                    .scope_for(*key)
                    .and_then(ParticipantScope::source_fence)
                    .is_some()
            })
            .expect("source-fenced coordinator");
        let stale_rows = coordinator_manifest
            .keys()
            .iter()
            .map(|key| {
                let outcome = if *key == coordinator_key {
                    CancellationCoordinatorClosureOutcome::Observed {
                        evidence: evidence_for(&progress, *key, 8),
                        context: CompletionContext::new(
                            progress.execution_epoch(),
                            progress.scope_for(*key).expect("coordinator scope").clone(),
                        ),
                    }
                } else {
                    CancellationCoordinatorClosureOutcome::Pending
                };
                CancellationCoordinatorClosureObservationRow::new(*key, outcome)
            })
            .collect();
        let stale = CancellationCoordinatorClosureObservationRecord::new(
            coordinator_manifest,
            dispatch.observation_epoch(),
            1,
            stale_rows,
        )
        .expect("stale coordinator receipt shape");
        assert_eq!(
            progress.validate_cancellation_coordinator_closure_observation_record(
                &stale,
                &source_observation,
                &recovery,
                &dispatch,
            ),
            Err(WorkProgressError::SourceRevisionRegressed {
                role: coordinator_key.role(),
                observed: 8,
                minimum: 10,
            })
        );
    }

    #[test]
    fn cancellation_effect_recovery_retains_merge_and_publish_late_facts() {
        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let mut progress = claimed_progress(
            participants,
            retained_resolution_args(source, source_id, true),
            1304,
        );
        for role in [WorkUnitRole::SourceRead, WorkUnitRole::Prepare] {
            apply_role(&mut progress, 0, role);
        }
        progress
            .request_cancellation(CancellationRequestObservation::new(1304).expect("cancel"))
            .expect("retained merge cancellation");
        let merge = key_for(&progress, 0, WorkUnitRole::RetainedMerge);
        let publish = key_for(&progress, 0, WorkUnitRole::Publish);
        let observations = [
            (
                merge,
                evidence_for(&progress, merge, 7),
                CompletionContext::new(
                    progress.execution_epoch(),
                    progress.scope_for(merge).expect("merge scope").clone(),
                ),
            ),
            (
                publish,
                evidence_for(&progress, publish, 7),
                CompletionContext::new(
                    progress.execution_epoch(),
                    progress.scope_for(publish).expect("publish scope").clone(),
                ),
            ),
        ];
        let record = recovery_dispatch_record(&progress, 1, &observations);
        let recovery = progress
            .cancellation_effect_recovery_inventory(&record)
            .expect("retained merge recovery effects");
        assert_eq!(recovery.bundles().len(), 1);
        assert_eq!(
            recovery.bundles()[0].recipe(),
            CancellationEffectRecipe::RetainedMerge
        );
        assert_eq!(
            recovery.bundles()[0]
                .facts()
                .iter()
                .map(|fact| fact.kind())
                .collect::<Vec<_>>(),
            vec![
                CancellationEffectFactKind::RetainedMerge,
                CancellationEffectFactKind::Publish,
            ]
        );
    }

    #[test]
    fn cancellation_effect_recovery_keeps_preparation_and_uncertainty_without_bundles() {
        let mut progress = copy_progress_with_admission_claims();
        progress
            .request_cancellation(CancellationRequestObservation::new(1305).expect("cancel"))
            .expect("copy cancellation");
        let frozen_inventory = progress
            .cancellation_inventory()
            .expect("frozen cancellation inventory")
            .clone();
        let uncertain = recovery_dispatch_record(&progress, 1, &[]);
        let uncertain_recovery = progress
            .cancellation_effect_recovery_inventory(&uncertain)
            .expect("uncertain recovery effects");
        assert!(uncertain_recovery.bundles().is_empty());
        assert_eq!(uncertain_recovery.frozen_inventory(), &frozen_inventory);
        assert!(
            uncertain_recovery
                .dispatch_outcome()
                .rows()
                .iter()
                .all(|row| matches!(row.outcome(), CancellationDispatchOutcome::EntryUncertain))
        );

        let prepare = key_for(&progress, 0, WorkUnitRole::Prepare);
        let observed = recovery_dispatch_record(
            &progress,
            2,
            &[(
                prepare,
                evidence_for(&progress, prepare, 7),
                CompletionContext::new(
                    progress.execution_epoch(),
                    progress.scope_for(prepare).expect("prepare scope").clone(),
                ),
            )],
        );
        let preparation_recovery = progress
            .cancellation_effect_recovery_inventory(&observed)
            .expect("preparation-only recovery effects");
        assert!(preparation_recovery.bundles().is_empty());
        assert_eq!(
            preparation_recovery
                .dispatch_outcome()
                .rows()
                .iter()
                .find(|row| row.key() == prepare)
                .expect("preparation row")
                .outcome(),
            &CancellationDispatchOutcome::ObservedAfterCancellation {
                evidence: evidence_for(&progress, prepare, 7),
                context: CompletionContext::new(
                    progress.execution_epoch(),
                    progress.scope_for(prepare).expect("prepare scope").clone(),
                ),
            }
        );
        assert_eq!(preparation_recovery.frozen_inventory(), &frozen_inventory);
        let preparation_cleanup = progress
            .cancellation_cleanup_manifest(&preparation_recovery, &observed)
            .expect("preparation cleanup manifest");
        assert_eq!(preparation_cleanup.segments().len(), 1);
        assert_eq!(
            preparation_cleanup.segments()[0].recipe(),
            CancellationEffectRecipe::StagedPublish
        );
        assert!(preparation_recovery.bundles().is_empty());
    }

    #[test]
    fn cancellation_cleanup_manifest_retains_all_recipe_segments_and_claims() {
        let mut staged_publish = copy_progress_with_admission_claims();
        let (_, _, staged_publish_manifest) = cleanup_manifest_for(&mut staged_publish, 1500);
        assert_eq!(
            staged_publish_manifest.segments()[0].recipe(),
            CancellationEffectRecipe::StagedPublish
        );
        assert!(
            staged_publish_manifest.segments()[0]
                .keys()
                .iter()
                .any(|key| key.role() == WorkUnitRole::Stage)
        );
        assert!(
            staged_publish_manifest.segments()[0]
                .applicable_staging_key()
                .is_some()
        );

        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let mut staged_move = claimed_progress(
            participants.clone(),
            namespace_file_move_args(source, source_id.clone()),
            1501,
        );
        let (_, _, staged_move_manifest) = cleanup_manifest_for(&mut staged_move, 1501);
        assert_eq!(
            staged_move_manifest.segments()[0].recipe(),
            CancellationEffectRecipe::StagedMove
        );
        assert!(
            staged_move_manifest.segments()[0]
                .applicable_origin_source_reconcile_key()
                .is_some()
        );

        let mut qualified = claimed_progress(
            participants.clone(),
            history_folder_args(source, source_id.clone()),
            1502,
        );
        let (_, _, qualified_manifest) = cleanup_manifest_for(&mut qualified, 1502);
        assert_eq!(qualified_manifest.segments().len(), 2);
        assert!(
            qualified_manifest
                .segments()
                .iter()
                .all(|segment| segment.recipe() == CancellationEffectRecipe::QualifiedRelocation)
        );
        assert!(qualified_manifest.segments().iter().all(|segment| {
            segment
                .keys()
                .iter()
                .any(|key| key.role() == WorkUnitRole::Stage)
                && segment.applicable_staging_key().is_none()
        }));

        let mut retained_merge = claimed_progress(
            participants.clone(),
            retained_resolution_args(source, source_id.clone(), true),
            1503,
        );
        let (_, _, retained_merge_manifest) = cleanup_manifest_for(&mut retained_merge, 1503);
        assert_eq!(
            retained_merge_manifest.segments()[0].recipe(),
            CancellationEffectRecipe::RetainedMerge
        );

        let mut retained_disposition = claimed_progress(
            participants.clone(),
            retained_resolution_args(source, source_id.clone(), false),
            1504,
        );
        let (_, _, retained_disposition_manifest) =
            cleanup_manifest_for(&mut retained_disposition, 1504);
        assert_eq!(
            retained_disposition_manifest.segments()[0].recipe(),
            CancellationEffectRecipe::RetainedDisposition
        );
        assert!(
            retained_disposition_manifest.segments()[0]
                .applicable_selected_retained_object_disposition_key()
                .is_some()
        );

        let filesystem = participant_id(2);
        let mut external_audit = claimed_progress(
            participants.clone(),
            external_reconciliation_args(source, filesystem, source_id.clone()),
            1505,
        );
        let (_, _, external_audit_manifest) = cleanup_manifest_for(&mut external_audit, 1505);
        assert_eq!(
            external_audit_manifest.segments()[0].recipe(),
            CancellationEffectRecipe::ExternalAudit
        );

        let mut history_restore = claimed_progress(
            participants,
            history_restore_with_extracted_args(source, filesystem, source_id),
            1506,
        );
        let (_, _, history_restore_manifest) = cleanup_manifest_for(&mut history_restore, 1506);
        assert_eq!(history_restore_manifest.segments().len(), 2);
        assert_eq!(
            history_restore_manifest.segments()[0].recipe(),
            CancellationEffectRecipe::StagedPublish
        );
        assert_eq!(
            history_restore_manifest.segments()[1].recipe(),
            CancellationEffectRecipe::HistoryUndoExtractedRemoval
        );

        let mut claimed_recovery = copy_progress_with_two_admission_claims();
        let (_, _, claimed_recovery_manifest) = cleanup_manifest_for(&mut claimed_recovery, 1507);
        assert_eq!(
            claimed_recovery_manifest.operation_recovery_payload_claim_ids(),
            &[AdmissionClaimId::from_uuid(Uuid::from_u128(202)).expect("recovery claim")]
        );
        assert!(
            !claimed_recovery_manifest
                .segments()
                .iter()
                .flat_map(CancellationCleanupSegment::keys)
                .any(|key| key.role() == WorkUnitRole::RecoveryMaterialRetained)
        );
    }

    #[test]
    fn cancellation_cleanup_manifest_keeps_pending_segments_and_rejects_malformed_candidates() {
        let mut progress = copy_progress_with_admission_claims();
        let (dispatch, recovery, valid) = cleanup_manifest_for(&mut progress, 1508);
        let steps_before = progress.steps().to_vec();
        let inventory_before = progress.cancellation_inventory().cloned();
        assert!(
            valid
                .segments()
                .iter()
                .all(|segment| segment.keys().iter().all(|key| {
                    progress
                        .cancellation_inventory()
                        .expect("inventory")
                        .steps()
                        .iter()
                        .any(|snapshot| snapshot.key() == *key)
                }))
        );
        assert_eq!(progress.steps(), steps_before.as_slice());
        assert_eq!(progress.cancellation_inventory(), inventory_before.as_ref());

        let missing = CancellationCleanupManifest::new(
            valid.recovery_inventory().clone(),
            dispatch.clone(),
            Vec::new(),
            valid.operation_recovery_payload_claim_ids().to_vec(),
        )
        .expect("missing candidate shape");
        assert_eq!(
            progress.validate_cancellation_cleanup_manifest(&missing, &recovery, &dispatch),
            Err(WorkProgressError::CancellationCleanupManifest(
                CancellationCleanupManifestError::CandidateMismatch
            ))
        );

        let different_dispatch = recovery_dispatch_record(&progress, 2, &[]);
        assert!(matches!(
            progress
                .validate_cancellation_cleanup_manifest(&valid, &recovery, &different_dispatch,),
            Err(WorkProgressError::CancellationEffectInventory(
                CancellationEffectInventoryError::DispatchOutcomeMismatch
            ))
        ));

        let original = valid.segments()[0].clone();
        let wrong_recipe = CancellationCleanupSegment::new(
            original.action_ordinal(),
            original.step_segment(),
            CancellationEffectRecipe::ExternalAudit,
            original.keys().to_vec(),
            original.applicable_keys().to_vec(),
        )
        .expect("wrong recipe candidate shape");
        let altered = CancellationCleanupManifest::new(
            valid.recovery_inventory().clone(),
            dispatch.clone(),
            vec![wrong_recipe],
            valid.operation_recovery_payload_claim_ids().to_vec(),
        )
        .expect("altered candidate shape");
        assert_eq!(
            progress.validate_cancellation_cleanup_manifest(&altered, &recovery, &dispatch),
            Err(WorkProgressError::CancellationCleanupManifest(
                CancellationCleanupManifestError::CandidateMismatch
            ))
        );

        let extra_key = WorkUnitKey::new(
            1,
            original.keys()[0].step_ordinal(),
            original.keys()[0].participant_id(),
            original.keys()[0].role(),
        );
        let extra_segment = CancellationCleanupSegment::new(
            1,
            0,
            original.recipe(),
            vec![extra_key],
            vec![extra_key],
        )
        .expect("extra segment candidate shape");
        let extra = CancellationCleanupManifest::new(
            valid.recovery_inventory().clone(),
            dispatch.clone(),
            vec![original.clone(), extra_segment],
            valid.operation_recovery_payload_claim_ids().to_vec(),
        )
        .expect("extra segment candidate");
        assert_eq!(
            progress.validate_cancellation_cleanup_manifest(&extra, &recovery, &dispatch),
            Err(WorkProgressError::CancellationCleanupManifest(
                CancellationCleanupManifestError::CandidateMismatch
            ))
        );
        assert!(matches!(
            CancellationCleanupManifest::new(
                valid.recovery_inventory().clone(),
                dispatch.clone(),
                vec![original.clone(), original.clone()],
                valid.operation_recovery_payload_claim_ids().to_vec(),
            ),
            Err(CancellationCleanupManifestValidationError::SegmentsNotSortedOrUnique { .. })
        ));

        let mut reversed_keys = original.keys().to_vec();
        reversed_keys.reverse();
        assert!(matches!(
            CancellationCleanupSegment::new(
                original.action_ordinal(),
                original.step_segment(),
                original.recipe(),
                reversed_keys,
                original.applicable_keys().to_vec(),
            ),
            Err(CancellationCleanupManifestValidationError::KeysNotSortedOrUnique { .. })
        ));

        let (other_participants, other_source, _other_filesystem, other_source_id) =
            valid_topology(false);
        let mut other_progress = claimed_progress(
            other_participants,
            copy_args_at(other_source, other_source_id, "other.wav"),
            1510,
        );
        let (_other_dispatch, other_recovery, other_manifest) =
            cleanup_manifest_for(&mut other_progress, 1509);
        assert_eq!(
            other_recovery.frozen_inventory().operation_id(),
            other_progress.operation_id()
        );
        assert_eq!(
            progress.validate_cancellation_cleanup_manifest(&other_manifest, &recovery, &dispatch,),
            Err(WorkProgressError::CancellationCleanupManifest(
                CancellationCleanupManifestError::RecoveryInventoryMismatch
            ))
        );
    }

    #[test]
    fn cancellation_resource_cleanup_manifest_covers_required_resources_only() {
        let (participants, source, filesystem, source_id) = valid_topology(false);
        let mut progress = claimed_progress(
            participants,
            history_restore_with_extracted_args(source, filesystem, source_id),
            1520,
        );
        let (dispatch, recovery, cleanup, resource) =
            resource_cleanup_manifest_for(&mut progress, 1520);
        assert!(!resource.keys().is_empty());
        assert!(resource.keys().iter().all(|key| {
            matches!(
                key.role(),
                WorkUnitRole::Stage | WorkUnitRole::RecoveryMaterialRetained
            )
        }));
        assert!(
            resource
                .keys()
                .iter()
                .any(|key| key.role() == WorkUnitRole::RecoveryMaterialRetained)
        );
        assert!(
            resource
                .cleanup_manifest()
                .operation_recovery_payload_claim_ids()
                .iter()
                .all(|claim| cleanup
                    .operation_recovery_payload_claim_ids()
                    .contains(claim))
        );
        let steps_before = progress.steps().to_vec();
        let inventory_before = progress.cancellation_inventory().cloned();
        let pending = CancellationResourceCleanupObservationRecord::new(
            resource.clone(),
            progress.execution_epoch(),
            1,
            pending_resource_cleanup_rows(&resource),
        )
        .expect("pending resource cleanup record");
        progress
            .validate_cancellation_resource_cleanup_observation_record(
                &pending, &cleanup, &recovery, &dispatch,
            )
            .expect("pending resource cleanup validation");
        assert_eq!(progress.steps(), steps_before.as_slice());
        assert_eq!(progress.cancellation_inventory(), inventory_before.as_ref());

        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let parent = NamespaceDirectoryEndpoint::new(
            source,
            source_id.clone(),
            NamespaceDirectoryLocator::Root,
            NamespaceDirectoryExpectation::new(7, "root-a", Some(1)).expect("root"),
        )
        .expect("parent");
        let args = UniversalCommandArgs::Namespace(NamespaceCommandArgs::CreateFolder(
            CreateFolderIntentArgs::new(parent, NamespaceLeafName::new("created").expect("leaf"))
                .expect("create-folder arguments"),
        ));
        let plan = super::super::work_plan::derive_work_plan(&args, &participants)
            .expect("create-folder plan");
        let mut create_folder = WorkProgress::from_plan_with_admission_observations(
            OperationId::from_uuid(Uuid::from_u128(1521)),
            &plan,
            &args,
            &participants,
            &admission_capacity_plan(),
            admission_claims(12),
        )
        .expect("create-folder progress");
        create_folder
            .request_cancellation(CancellationRequestObservation::new(1521).expect("cancel"))
            .expect("create-folder cancellation");
        let create_dispatch = all_never_dispatch_record(&create_folder);
        let create_recovery = create_folder
            .cancellation_effect_recovery_inventory(&create_dispatch)
            .expect("create-folder recovery");
        let create_cleanup = create_folder
            .cancellation_cleanup_manifest(&create_recovery, &create_dispatch)
            .expect("create-folder cleanup");
        let create_resource = create_folder
            .cancellation_resource_cleanup_manifest(
                &create_cleanup,
                &create_recovery,
                &create_dispatch,
            )
            .expect("create-folder resource cleanup");
        assert!(create_resource.keys().is_empty());
        let create_record = CancellationResourceCleanupObservationRecord::new(
            create_resource.clone(),
            create_folder.execution_epoch(),
            1,
            Vec::new(),
        )
        .expect("empty create-folder resource record");
        create_folder
            .validate_cancellation_resource_cleanup_observation_record(
                &create_record,
                &create_cleanup,
                &create_recovery,
                &create_dispatch,
            )
            .expect("empty create-folder resource validation");
        assert!(matches!(
            CancellationResourceCleanupManifest::new(
                create_cleanup,
                vec![key_for(&create_folder, 0, WorkUnitRole::CopyValidated)],
            ),
            Err(CancellationResourceCleanupManifestValidationError::UnexpectedRole)
        ));
    }

    #[test]
    fn cancellation_resource_cleanup_observations_preserve_context_after_reacquisition() {
        let (participants, source, filesystem, source_id) = valid_topology(false);
        let mut progress = claimed_progress(
            participants,
            history_restore_with_extracted_args(source, filesystem, source_id),
            1522,
        );
        let (dispatch, recovery, cleanup, resource) =
            resource_cleanup_manifest_for(&mut progress, 1522);
        let rows = resource
            .keys()
            .iter()
            .enumerate()
            .map(|(index, key)| {
                let context = CompletionContext::new(
                    progress.execution_epoch(),
                    progress.scope_for(*key).expect("resource scope").clone(),
                );
                let outcome = if index % 2 == 0 {
                    CancellationResourceCleanupOutcome::Preserved {
                        owner_revision: u64::MAX - index as u64,
                        context,
                    }
                } else {
                    CancellationResourceCleanupOutcome::Absent {
                        owner_revision: u64::MAX - index as u64,
                        context,
                    }
                };
                CancellationResourceCleanupObservationRow::new(*key, outcome)
            })
            .collect();
        let record = CancellationResourceCleanupObservationRecord::new(
            resource.clone(),
            progress.execution_epoch(),
            1,
            rows,
        )
        .expect("resource observation record");
        progress
            .validate_cancellation_resource_cleanup_observation_record(
                &record, &cleanup, &recovery, &dispatch,
            )
            .expect("resource observation validation");

        let steps_before = progress.steps().to_vec();
        let inventory_before = progress.cancellation_inventory().cloned();
        let source_key = key_for(&progress, 0, WorkUnitRole::SourceRead);
        let source_participant = source_key.participant_id();
        progress
            .reacquire_source(source_reacquisition_authority(
                &progress,
                source_participant,
                7,
                2,
                0,
            ))
            .expect("source reacquisition");
        progress
            .validate_cancellation_resource_cleanup_observation_record(
                &record, &cleanup, &recovery, &dispatch,
            )
            .expect("historical resource context remains valid");
        assert_eq!(progress.steps(), steps_before.as_slice());
        assert_eq!(progress.cancellation_inventory(), inventory_before.as_ref());

        let key = resource.keys()[0];
        let mut wrong_context =
            CompletionContext::new(0, progress.scope_for(key).expect("resource scope").clone());
        wrong_context.scope.source_fence = progress
            .scope_for(source_key)
            .and_then(ParticipantScope::source_fence)
            .cloned();
        let wrong_rows = resource
            .keys()
            .iter()
            .map(|candidate| {
                let context = if *candidate == key {
                    wrong_context.clone()
                } else {
                    CompletionContext::new(
                        0,
                        progress
                            .scope_for(*candidate)
                            .expect("resource scope")
                            .clone(),
                    )
                };
                CancellationResourceCleanupObservationRow::new(
                    *candidate,
                    CancellationResourceCleanupOutcome::Preserved {
                        owner_revision: 1,
                        context,
                    },
                )
            })
            .collect();
        let wrong = CancellationResourceCleanupObservationRecord::new(resource, 0, 1, wrong_rows)
            .expect("wrong-fence record shape");
        assert_eq!(
            progress.validate_cancellation_resource_cleanup_observation_record(
                &wrong, &cleanup, &recovery, &dispatch,
            ),
            Err(WorkProgressError::CompletionContextScopeMismatch { key })
        );
    }

    #[test]
    fn cancellation_resource_cleanup_observation_rejects_bad_coverage_and_headers() {
        let (participants, source, filesystem, source_id) = valid_topology(false);
        let mut progress = claimed_progress(
            participants,
            history_restore_with_extracted_args(source, filesystem, source_id),
            1523,
        );
        let (dispatch, recovery, cleanup, resource) =
            resource_cleanup_manifest_for(&mut progress, 1523);
        let rows = pending_resource_cleanup_rows(&resource);
        let zero_generation = CancellationResourceCleanupObservationRecord::new(
            resource.clone(),
            progress.execution_epoch(),
            0,
            rows.clone(),
        );
        assert_eq!(
            zero_generation,
            Err(CancellationResourceCleanupObservationRecordError::ObservationGenerationZero)
        );
        if let Some(key) = resource.keys().first() {
            let invalid = CancellationResourceCleanupObservationRecord::new(
                resource.clone(),
                progress.execution_epoch(),
                1,
                resource
                    .keys()
                    .iter()
                    .map(|candidate| {
                        CancellationResourceCleanupObservationRow::new(
                            *candidate,
                            if candidate == key {
                                CancellationResourceCleanupOutcome::Absent {
                                    owner_revision: 0,
                                    context: CompletionContext::new(
                                        progress.execution_epoch(),
                                        progress
                                            .scope_for(*candidate)
                                            .expect("resource scope")
                                            .clone(),
                                    ),
                                }
                            } else {
                                CancellationResourceCleanupOutcome::Pending
                            },
                        )
                    })
                    .collect(),
            );
            assert_eq!(
                invalid,
                Err(
                    CancellationResourceCleanupObservationRecordError::OwnerRevisionZero {
                        key: *key
                    }
                )
            );
        }
        let future = CancellationResourceCleanupObservationRecord::new(
            resource.clone(),
            progress.execution_epoch() + 1,
            1,
            rows.clone(),
        )
        .expect("future header shape");
        assert_eq!(
            progress.validate_cancellation_resource_cleanup_observation_record(
                &future, &cleanup, &recovery, &dispatch,
            ),
            Err(WorkProgressError::CancellationResourceCleanupObservation(
                CancellationResourceCleanupObservationRecordError::ObservationEpochFuture {
                    actual: 1,
                    current: 0,
                }
            ))
        );

        let missing = CancellationResourceCleanupObservationRecord::new(
            resource.clone(),
            progress.execution_epoch(),
            1,
            rows.iter().skip(1).cloned().collect(),
        )
        .expect("missing row shape");
        assert!(matches!(
            progress.validate_cancellation_resource_cleanup_observation_record(
                &missing, &cleanup, &recovery, &dispatch,
            ),
            Err(WorkProgressError::CancellationResourceCleanupObservation(
                CancellationResourceCleanupObservationRecordError::RowCountMismatch { .. }
            ))
        ));

        let extra_key =
            WorkUnitKey::new(u8::MAX, u16::MAX, participant_id(999), WorkUnitRole::Stage);
        let mut extra_rows = rows.clone();
        extra_rows.push(CancellationResourceCleanupObservationRow::new(
            extra_key,
            CancellationResourceCleanupOutcome::Pending,
        ));
        let extra = CancellationResourceCleanupObservationRecord::new(
            resource.clone(),
            progress.execution_epoch(),
            1,
            extra_rows,
        )
        .expect("extra row shape");
        assert!(matches!(
            progress.validate_cancellation_resource_cleanup_observation_record(
                &extra, &cleanup, &recovery, &dispatch,
            ),
            Err(WorkProgressError::CancellationResourceCleanupObservation(
                CancellationResourceCleanupObservationRecordError::RowCountMismatch { .. }
            ))
        ));

        let mut reordered_rows = rows.clone();
        reordered_rows.reverse();
        assert!(matches!(
            CancellationResourceCleanupObservationRecord::new(
                resource.clone(),
                progress.execution_epoch(),
                1,
                reordered_rows,
            ),
            Err(CancellationResourceCleanupObservationRecordError::RowsNotSortedOrUnique { .. })
        ));

        if let Some(key) = resource.keys().first() {
            let future_context = CancellationResourceCleanupObservationRecord::new(
                resource.clone(),
                progress.execution_epoch(),
                1,
                resource
                    .keys()
                    .iter()
                    .map(|candidate| {
                        CancellationResourceCleanupObservationRow::new(
                            *candidate,
                            if candidate == key {
                                CancellationResourceCleanupOutcome::Preserved {
                                    owner_revision: 1,
                                    context: CompletionContext::new(
                                        progress.execution_epoch() + 1,
                                        progress
                                            .scope_for(*candidate)
                                            .expect("resource scope")
                                            .clone(),
                                    ),
                                }
                            } else {
                                CancellationResourceCleanupOutcome::Pending
                            },
                        )
                    })
                    .collect(),
            );
            assert_eq!(
                future_context,
                Err(
                    CancellationResourceCleanupObservationRecordError::
                        ContextEpochAfterObservation {
                            key: *key,
                            context_epoch: 1,
                            observation_epoch: 0,
                        },
                )
            );
        }

        let other = {
            let (participants, source, _filesystem, source_id) = valid_topology(false);
            let mut other = claimed_progress(
                participants,
                copy_args_at(source, source_id, "other-resource.wav"),
                1524,
            );
            resource_cleanup_manifest_for(&mut other, 1524).3
        };
        let wrong_lineage = CancellationResourceCleanupObservationRecord::new(
            other,
            progress.execution_epoch(),
            1,
            rows,
        )
        .expect("wrong-lineage record shape");
        assert!(matches!(
            progress.validate_cancellation_resource_cleanup_observation_record(
                &wrong_lineage,
                &cleanup,
                &recovery,
                &dispatch,
            ),
            Err(WorkProgressError::CancellationResourceCleanupObservation(
                CancellationResourceCleanupObservationRecordError::ManifestMismatch
            ))
        ));
    }

    #[test]
    fn cancellation_namespace_cleanup_manifest_covers_all_recipe_shapes() {
        let assert_roles = |manifest: &CancellationNamespaceCleanupManifest| {
            assert!(manifest.keys().iter().all(|key| {
                matches!(
                    key.role(),
                    WorkUnitRole::Publish
                        | WorkUnitRole::QualifiedRelocation
                        | WorkUnitRole::RetainedMerge
                        | WorkUnitRole::OriginAbsence
                        | WorkUnitRole::RetainedObjectAbsence
                        | WorkUnitRole::RestoreRecordPrepared
                )
            }));
            assert!(manifest.keys().iter().all(|key| {
                !matches!(
                    key.role(),
                    WorkUnitRole::OriginRemovalStart
                        | WorkUnitRole::SelectedRetainedObjectDisposition
                        | WorkUnitRole::ExternalAudit
                )
            }));
        };

        let mut staged_publish = copy_progress_with_admission_claims();
        let (_, _, _, staged_publish_namespace) =
            namespace_cleanup_manifest_for(&mut staged_publish, 1530);
        assert_roles(&staged_publish_namespace);
        assert!(
            staged_publish_namespace
                .keys()
                .iter()
                .any(|key| key.role() == WorkUnitRole::Publish)
        );

        let (participants, source, filesystem, source_id) = valid_topology(false);
        let mut staged_move = claimed_progress(
            participants.clone(),
            namespace_file_move_args(source, source_id.clone()),
            1531,
        );
        let (_, _, _, staged_move_namespace) =
            namespace_cleanup_manifest_for(&mut staged_move, 1531);
        assert_roles(&staged_move_namespace);
        assert!(
            staged_move_namespace
                .keys()
                .iter()
                .any(|key| key.role() == WorkUnitRole::Publish)
        );
        assert!(
            staged_move_namespace
                .keys()
                .iter()
                .any(|key| key.role() == WorkUnitRole::OriginAbsence)
        );

        let mut qualified = claimed_progress(
            participants.clone(),
            history_folder_args(source, source_id.clone()),
            1532,
        );
        let (_, _, _, qualified_namespace) = namespace_cleanup_manifest_for(&mut qualified, 1532);
        assert_roles(&qualified_namespace);
        assert!(
            qualified_namespace
                .keys()
                .iter()
                .any(|key| key.role() == WorkUnitRole::QualifiedRelocation)
        );
        assert!(
            qualified_namespace
                .keys()
                .iter()
                .any(|key| key.role() == WorkUnitRole::OriginAbsence)
        );

        let mut retained_merge = claimed_progress(
            participants.clone(),
            retained_resolution_args(source, source_id.clone(), true),
            1533,
        );
        let (_, _, _, retained_merge_namespace) =
            namespace_cleanup_manifest_for(&mut retained_merge, 1533);
        assert_roles(&retained_merge_namespace);
        assert!(
            retained_merge_namespace
                .keys()
                .iter()
                .any(|key| key.role() == WorkUnitRole::RetainedMerge)
        );

        let mut retained_disposition = claimed_progress(
            participants.clone(),
            retained_resolution_args(source, source_id.clone(), false),
            1534,
        );
        let (_, _, _, retained_disposition_namespace) =
            namespace_cleanup_manifest_for(&mut retained_disposition, 1534);
        assert_roles(&retained_disposition_namespace);
        assert!(
            retained_disposition_namespace
                .keys()
                .iter()
                .any(|key| key.role() == WorkUnitRole::RetainedObjectAbsence)
        );
        assert!(
            retained_disposition_namespace
                .keys()
                .iter()
                .all(|key| key.role() != WorkUnitRole::SelectedRetainedObjectDisposition)
        );

        let mut external_audit = claimed_progress(
            participants.clone(),
            external_reconciliation_args(source, filesystem, source_id.clone()),
            1535,
        );
        let (_, _, _, external_namespace) =
            namespace_cleanup_manifest_for(&mut external_audit, 1535);
        assert_roles(&external_namespace);
        assert!(external_namespace.keys().is_empty());

        let mut history_restore = claimed_progress(
            participants,
            history_restore_with_extracted_args(source, filesystem, source_id),
            1536,
        );
        let (_, _, _, history_namespace) =
            namespace_cleanup_manifest_for(&mut history_restore, 1536);
        assert_roles(&history_namespace);
        assert!(
            history_namespace
                .keys()
                .iter()
                .any(|key| key.role() == WorkUnitRole::Publish)
        );
        assert!(
            history_namespace
                .keys()
                .iter()
                .any(|key| key.role() == WorkUnitRole::OriginAbsence)
        );
        assert!(
            history_namespace.keys().len()
                < history_namespace
                    .cleanup_manifest()
                    .segments()
                    .iter()
                    .map(|segment| segment.keys().len())
                    .sum::<usize>()
        );
    }

    #[test]
    fn cancellation_namespace_cleanup_observations_enforce_roles_and_retain_contradictions() {
        let (participants, source, filesystem, source_id) = valid_topology(false);
        let mut progress = claimed_progress(
            participants,
            history_restore_with_extracted_args(source, filesystem, source_id),
            1537,
        );
        let (dispatch, recovery, cleanup, namespace) =
            namespace_cleanup_manifest_for(&mut progress, 1537);
        let rows: Vec<CancellationNamespaceCleanupObservationRow> = namespace
            .keys()
            .iter()
            .map(|key| {
                let context = CompletionContext::new(
                    progress.execution_epoch(),
                    progress.scope_for(*key).expect("namespace scope").clone(),
                );
                let outcome = match key.role() {
                    WorkUnitRole::Publish
                    | WorkUnitRole::QualifiedRelocation
                    | WorkUnitRole::RetainedMerge => {
                        CancellationNamespaceCleanupOutcome::NoOperationClaim {
                            owner_revision: 1,
                            context,
                        }
                    }
                    WorkUnitRole::OriginAbsence
                    | WorkUnitRole::RetainedObjectAbsence
                    | WorkUnitRole::RestoreRecordPrepared => {
                        CancellationNamespaceCleanupOutcome::Absent {
                            owner_revision: 1,
                            context,
                        }
                    }
                    role => panic!("unexpected namespace role {role:?}"),
                };
                CancellationNamespaceCleanupObservationRow::new(*key, outcome)
            })
            .collect();
        let record = CancellationNamespaceCleanupObservationRecord::new(
            namespace.clone(),
            progress.execution_epoch(),
            1,
            rows.clone(),
        )
        .expect("namespace observation record");
        let steps_before = progress.steps().to_vec();
        let inventory_before = progress.cancellation_inventory().cloned();
        let claims_before = progress.admission_claims().cloned();
        progress
            .validate_cancellation_namespace_cleanup_observation_record(
                &record, &cleanup, &recovery, &dispatch,
            )
            .expect("namespace observation validation");

        let contradiction_key = namespace
            .keys()
            .iter()
            .copied()
            .find(|key| {
                matches!(
                    key.role(),
                    WorkUnitRole::OriginAbsence
                        | WorkUnitRole::RetainedObjectAbsence
                        | WorkUnitRole::RestoreRecordPrepared
                )
            })
            .expect("absence-compatible namespace key");
        let contradiction_rows: Vec<CancellationNamespaceCleanupObservationRow> = rows
            .iter()
            .map(|row| {
                if row.key() == contradiction_key {
                    CancellationNamespaceCleanupObservationRow::new(
                        contradiction_key,
                        CancellationNamespaceCleanupOutcome::Preserved {
                            owner_revision: 2,
                            context: CompletionContext::new(
                                progress.execution_epoch(),
                                progress
                                    .scope_for(contradiction_key)
                                    .expect("contradiction scope")
                                    .clone(),
                            ),
                        },
                    )
                } else {
                    row.clone()
                }
            })
            .collect();
        let contradiction = CancellationNamespaceCleanupObservationRecord::new(
            namespace.clone(),
            progress.execution_epoch(),
            2,
            contradiction_rows,
        )
        .expect("contradictory namespace observation");
        progress
            .validate_cancellation_namespace_cleanup_observation_record(
                &contradiction,
                &cleanup,
                &recovery,
                &dispatch,
            )
            .expect("contradictory observation remains observational");
        assert_eq!(progress.steps(), steps_before.as_slice());
        assert_eq!(progress.cancellation_inventory(), inventory_before.as_ref());
        assert_eq!(progress.admission_claims(), claims_before.as_ref());

        let publish = namespace
            .keys()
            .iter()
            .copied()
            .find(|key| key.role() == WorkUnitRole::Publish)
            .expect("publish namespace key");
        let absent_on_publish: Vec<CancellationNamespaceCleanupObservationRow> = rows
            .iter()
            .map(|row| {
                if row.key() == publish {
                    CancellationNamespaceCleanupObservationRow::new(
                        publish,
                        CancellationNamespaceCleanupOutcome::Absent {
                            owner_revision: 1,
                            context: CompletionContext::new(
                                progress.execution_epoch(),
                                progress.scope_for(publish).expect("publish scope").clone(),
                            ),
                        },
                    )
                } else {
                    row.clone()
                }
            })
            .collect();
        assert_eq!(
            CancellationNamespaceCleanupObservationRecord::new(
                namespace.clone(),
                progress.execution_epoch(),
                1,
                absent_on_publish,
            ),
            Err(
                CancellationNamespaceCleanupObservationRecordError::AbsentNotAllowed {
                    key: publish,
                    role: WorkUnitRole::Publish,
                }
            )
        );

        let no_operation_on_absence: Vec<CancellationNamespaceCleanupObservationRow> = rows
            .iter()
            .map(|row| {
                if row.key() == contradiction_key {
                    CancellationNamespaceCleanupObservationRow::new(
                        contradiction_key,
                        CancellationNamespaceCleanupOutcome::NoOperationClaim {
                            owner_revision: 1,
                            context: CompletionContext::new(
                                progress.execution_epoch(),
                                progress
                                    .scope_for(contradiction_key)
                                    .expect("absence scope")
                                    .clone(),
                            ),
                        },
                    )
                } else {
                    row.clone()
                }
            })
            .collect();
        assert_eq!(
            CancellationNamespaceCleanupObservationRecord::new(
                namespace,
                progress.execution_epoch(),
                1,
                no_operation_on_absence,
            ),
            Err(
                CancellationNamespaceCleanupObservationRecordError::NoOperationClaimNotAllowed {
                    key: contradiction_key,
                    role: contradiction_key.role(),
                }
            )
        );
    }

    #[test]
    fn cancellation_namespace_cleanup_observations_preserve_context_and_reject_shape_errors() {
        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let mut progress = claimed_progress(
            participants,
            history_folder_args(source, source_id.clone()),
            1538,
        );
        let (dispatch, recovery, cleanup, namespace) =
            namespace_cleanup_manifest_for(&mut progress, 1538);
        let rows: Vec<CancellationNamespaceCleanupObservationRow> = namespace
            .keys()
            .iter()
            .map(|key| {
                CancellationNamespaceCleanupObservationRow::new(
                    *key,
                    CancellationNamespaceCleanupOutcome::Preserved {
                        owner_revision: u64::MAX,
                        context: CompletionContext::new(
                            progress.execution_epoch(),
                            progress.scope_for(*key).expect("namespace scope").clone(),
                        ),
                    },
                )
            })
            .collect();
        let record = CancellationNamespaceCleanupObservationRecord::new(
            namespace.clone(),
            progress.execution_epoch(),
            1,
            rows.clone(),
        )
        .expect("namespace context record");
        progress
            .validate_cancellation_namespace_cleanup_observation_record(
                &record, &cleanup, &recovery, &dispatch,
            )
            .expect("namespace context validation");

        let steps_before = progress.steps().to_vec();
        let inventory_before = progress.cancellation_inventory().cloned();
        let source_key = key_for(&progress, 0, WorkUnitRole::SourceRead);
        let source_participant = source_key.participant_id();
        progress
            .reacquire_source(source_reacquisition_authority(
                &progress,
                source_participant,
                7,
                2,
                0,
            ))
            .expect("source reacquisition");
        progress
            .validate_cancellation_namespace_cleanup_observation_record(
                &record, &cleanup, &recovery, &dispatch,
            )
            .expect("historical namespace context remains valid");
        assert_eq!(progress.steps(), steps_before.as_slice());
        assert_eq!(progress.cancellation_inventory(), inventory_before.as_ref());

        let key = namespace.keys()[0];
        let mut wrong_context =
            CompletionContext::new(0, progress.scope_for(key).expect("namespace scope").clone());
        wrong_context.scope.source_fence = progress
            .scope_for(source_key)
            .and_then(ParticipantScope::source_fence)
            .cloned();
        let wrong_rows = namespace
            .keys()
            .iter()
            .map(|candidate| {
                let context = if *candidate == key {
                    wrong_context.clone()
                } else {
                    CompletionContext::new(
                        0,
                        progress
                            .scope_for(*candidate)
                            .expect("namespace scope")
                            .clone(),
                    )
                };
                CancellationNamespaceCleanupObservationRow::new(
                    *candidate,
                    CancellationNamespaceCleanupOutcome::Preserved {
                        owner_revision: 1,
                        context,
                    },
                )
            })
            .collect();
        let wrong =
            CancellationNamespaceCleanupObservationRecord::new(namespace.clone(), 0, 1, wrong_rows)
                .expect("wrong-fence record shape");
        assert_eq!(
            progress.validate_cancellation_namespace_cleanup_observation_record(
                &wrong, &cleanup, &recovery, &dispatch,
            ),
            Err(WorkProgressError::CompletionContextScopeMismatch { key })
        );

        let zero_generation = CancellationNamespaceCleanupObservationRecord::new(
            namespace.clone(),
            progress.execution_epoch(),
            0,
            pending_namespace_cleanup_rows(&namespace),
        );
        assert_eq!(
            zero_generation,
            Err(CancellationNamespaceCleanupObservationRecordError::ObservationGenerationZero)
        );
        let zero_owner = namespace
            .keys()
            .iter()
            .map(|candidate| {
                CancellationNamespaceCleanupObservationRow::new(
                    *candidate,
                    if *candidate == *namespace.keys().first().expect("namespace key") {
                        CancellationNamespaceCleanupOutcome::Preserved {
                            owner_revision: 0,
                            context: CompletionContext::new(
                                progress.execution_epoch(),
                                progress
                                    .scope_for(*candidate)
                                    .expect("namespace scope")
                                    .clone(),
                            ),
                        }
                    } else {
                        CancellationNamespaceCleanupOutcome::Pending
                    },
                )
            })
            .collect();
        assert_eq!(
            CancellationNamespaceCleanupObservationRecord::new(
                namespace.clone(),
                progress.execution_epoch(),
                1,
                zero_owner,
            ),
            Err(
                CancellationNamespaceCleanupObservationRecordError::OwnerRevisionZero {
                    key: namespace.keys()[0],
                }
            )
        );
        let missing = CancellationNamespaceCleanupObservationRecord::new(
            namespace.clone(),
            0,
            1,
            rows.iter().skip(1).cloned().collect(),
        )
        .expect("missing row shape");
        assert!(matches!(
            progress.validate_cancellation_namespace_cleanup_observation_record(
                &missing, &cleanup, &recovery, &dispatch,
            ),
            Err(WorkProgressError::CancellationNamespaceCleanupObservation(
                CancellationNamespaceCleanupObservationRecordError::RowCountMismatch { .. }
            ))
        ));
        let extra_key = WorkUnitKey::new(
            u8::MAX,
            u16::MAX,
            participant_id(999),
            WorkUnitRole::Publish,
        );
        let mut extra_rows = pending_namespace_cleanup_rows(&namespace);
        extra_rows.push(CancellationNamespaceCleanupObservationRow::new(
            extra_key,
            CancellationNamespaceCleanupOutcome::Pending,
        ));
        let extra =
            CancellationNamespaceCleanupObservationRecord::new(namespace.clone(), 0, 1, extra_rows)
                .expect("extra row shape");
        assert!(matches!(
            progress.validate_cancellation_namespace_cleanup_observation_record(
                &extra, &cleanup, &recovery, &dispatch,
            ),
            Err(WorkProgressError::CancellationNamespaceCleanupObservation(
                CancellationNamespaceCleanupObservationRecordError::RowCountMismatch { .. }
            ))
        ));

        let mut reordered = rows;
        reordered.reverse();
        assert!(matches!(
            CancellationNamespaceCleanupObservationRecord::new(namespace, 0, 1, reordered,),
            Err(CancellationNamespaceCleanupObservationRecordError::RowsNotSortedOrUnique { .. })
        ));

        let (participants, source, source_id) = {
            let (participants, source, _filesystem, source_id) = valid_topology(false);
            (participants, source, source_id)
        };
        let mut other =
            claimed_progress(participants, history_folder_args(source, source_id), 1539);
        let (_, _, _, other_namespace) = namespace_cleanup_manifest_for(&mut other, 1539);
        let wrong_lineage = CancellationNamespaceCleanupObservationRecord::new(
            other_namespace.clone(),
            0,
            1,
            pending_namespace_cleanup_rows(&other_namespace),
        )
        .expect("wrong-lineage record shape");
        assert!(matches!(
            progress.validate_cancellation_namespace_cleanup_observation_record(
                &wrong_lineage,
                &cleanup,
                &recovery,
                &dispatch,
            ),
            Err(WorkProgressError::CancellationNamespaceCleanupObservation(
                CancellationNamespaceCleanupObservationRecordError::ManifestMismatch
            ))
        ));
    }

    #[test]
    fn cancellation_effect_recovery_candidate_validation_rejects_row_changes() {
        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let mut progress = claimed_progress(
            participants,
            retained_resolution_args(source, source_id, true),
            1306,
        );
        for role in [WorkUnitRole::SourceRead, WorkUnitRole::Prepare] {
            apply_role(&mut progress, 0, role);
        }
        progress
            .request_cancellation(CancellationRequestObservation::new(1306).expect("cancel"))
            .expect("retained merge cancellation");
        let merge = key_for(&progress, 0, WorkUnitRole::RetainedMerge);
        let publish = key_for(&progress, 0, WorkUnitRole::Publish);
        let record = recovery_dispatch_record(
            &progress,
            1,
            &[
                (
                    merge,
                    evidence_for(&progress, merge, 7),
                    CompletionContext::new(
                        progress.execution_epoch(),
                        progress.scope_for(merge).expect("merge scope").clone(),
                    ),
                ),
                (
                    publish,
                    evidence_for(&progress, publish, 7),
                    CompletionContext::new(
                        progress.execution_epoch(),
                        progress.scope_for(publish).expect("publish scope").clone(),
                    ),
                ),
            ],
        );
        let valid = progress
            .cancellation_effect_recovery_inventory(&record)
            .expect("valid recovery effects");
        progress
            .validate_cancellation_effect_recovery_inventory(&valid, &record)
            .expect("valid candidate");
        let bundle = valid.bundles()[0].clone();

        let missing = CancellationEffectRecoveryInventory::new(
            valid.frozen_inventory().clone(),
            valid.dispatch_outcome().clone(),
            Vec::new(),
        )
        .expect("missing bundle candidate shape");
        assert_eq!(
            progress.validate_cancellation_effect_recovery_inventory(&missing, &record),
            Err(WorkProgressError::CancellationEffectInventory(
                CancellationEffectInventoryError::CandidateMismatch
            ))
        );

        let extra_bundle = CancellationEffectBundle::new(
            1,
            0,
            bundle.recipe(),
            bundle.outcome(),
            bundle.facts().to_vec(),
            bundle.source_closure().to_vec(),
        )
        .expect("extra bundle shape");
        let extra = CancellationEffectRecoveryInventory::new(
            valid.frozen_inventory().clone(),
            valid.dispatch_outcome().clone(),
            vec![bundle.clone(), extra_bundle],
        )
        .expect("extra bundle candidate shape");
        assert_eq!(
            progress.validate_cancellation_effect_recovery_inventory(&extra, &record),
            Err(WorkProgressError::CancellationEffectInventory(
                CancellationEffectInventoryError::CandidateMismatch
            ))
        );

        let mut altered_facts = bundle.facts().to_vec();
        altered_facts.pop();
        let altered_fact_bundle = CancellationEffectBundle::new(
            bundle.action_ordinal(),
            bundle.step_segment(),
            bundle.recipe(),
            bundle.outcome(),
            altered_facts,
            bundle.source_closure().to_vec(),
        )
        .expect("altered fact bundle shape");
        let altered_fact = CancellationEffectRecoveryInventory::new(
            valid.frozen_inventory().clone(),
            valid.dispatch_outcome().clone(),
            vec![altered_fact_bundle],
        )
        .expect("altered fact candidate shape");
        assert_eq!(
            progress.validate_cancellation_effect_recovery_inventory(&altered_fact, &record),
            Err(WorkProgressError::CancellationEffectInventory(
                CancellationEffectInventoryError::CandidateMismatch
            ))
        );

        let altered_closure_bundle = CancellationEffectBundle::new(
            bundle.action_ordinal(),
            bundle.step_segment(),
            bundle.recipe(),
            bundle.outcome(),
            bundle.facts().to_vec(),
            Vec::new(),
        )
        .expect("altered closure bundle shape");
        let altered_closure = CancellationEffectRecoveryInventory::new(
            valid.frozen_inventory().clone(),
            valid.dispatch_outcome().clone(),
            vec![altered_closure_bundle],
        )
        .expect("altered closure candidate shape");
        assert_eq!(
            progress.validate_cancellation_effect_recovery_inventory(&altered_closure, &record),
            Err(WorkProgressError::CancellationEffectInventory(
                CancellationEffectInventoryError::CandidateMismatch
            ))
        );
    }

    #[test]
    fn cancellation_effect_recovery_validation_uses_explicit_expected_record() {
        let mut progress = copy_progress_with_admission_claims();
        progress
            .request_cancellation(CancellationRequestObservation::new(1309).expect("cancel"))
            .expect("cancellation request");
        let old_record = recovery_dispatch_record(&progress, 1, &[]);
        let mut authority = pre_publish_cleanup_authority(
            &progress,
            PrePublishCleanupObservation::new(Vec::new()).expect("cleanup"),
            settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Held),
        );
        authority.dispatch_outcome = old_record.clone();
        progress
            .finish_pre_publish_cancellation(authority)
            .expect("retain original dispatch record");

        let observed_key = key_for(&progress, 0, WorkUnitRole::Stage);
        let new_record = recovery_dispatch_record(
            &progress,
            2,
            &[(
                observed_key,
                evidence_for(&progress, observed_key, 7),
                CompletionContext::new(
                    progress.execution_epoch(),
                    progress
                        .scope_for(observed_key)
                        .expect("stage scope")
                        .clone(),
                ),
            )],
        );
        let candidate = progress
            .cancellation_effect_recovery_inventory(&new_record)
            .expect("new observational record");
        progress
            .validate_cancellation_effect_recovery_inventory(&candidate, &new_record)
            .expect("new expected record validates");
        assert_eq!(
            progress.validate_cancellation_effect_recovery_inventory(&candidate, &old_record),
            Err(WorkProgressError::CancellationEffectInventory(
                CancellationEffectInventoryError::DispatchOutcomeMismatch
            ))
        );
    }

    #[test]
    fn cancellation_dispatch_manifest_includes_later_history_and_global_pending_work() {
        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let mut progress =
            claimed_progress(participants, history_folder_args(source, source_id), 317);
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::Prepare,
            WorkUnitRole::EnsureContainedParentDirectories,
            WorkUnitRole::QualifiedRelocation,
            WorkUnitRole::OriginAbsence,
            WorkUnitRole::SourceAffect,
        ] {
            apply_step(&mut progress, 0, 0, role);
        }
        progress
            .request_cancellation(CancellationRequestObservation::new(317).expect("cancel"))
            .expect("history cancellation");

        let inventory = progress
            .cancellation_inventory()
            .expect("cancellation inventory");
        let manifest = progress
            .cancellation_dispatch_manifest()
            .expect("dispatch manifest");
        assert_eq!(manifest.operation_id(), progress.operation_id());
        assert_eq!(manifest.plan_version(), progress.plan().plan_version());
        assert_eq!(
            manifest.frozen_inventory_generation(),
            inventory.generation()
        );
        assert_eq!(manifest.capture_epoch(), inventory.execution_epoch());
        assert!(manifest.keys().iter().any(|key| {
            key.action_ordinal() == 0
                && key.step_ordinal() / super::super::work_plan::STEP_HISTORY_STRIDE == 1
                && key.role() == WorkUnitRole::QualifiedRelocation
        }));
        assert!(
            manifest.keys().iter().any(|key| {
                key.action_ordinal() == super::super::work_plan::GLOBAL_ACTION_ORDINAL
            })
        );
        assert!(
            manifest
                .keys()
                .iter()
                .all(|key| key.role() != WorkUnitRole::SourceRead)
        );
        for key in manifest.keys() {
            let unit = progress
                .plan()
                .units()
                .iter()
                .find(|unit| unit.key() == *key)
                .expect("manifest plan key");
            let snapshot = inventory
                .steps()
                .iter()
                .find(|snapshot| snapshot.key() == *key)
                .expect("manifest inventory key");
            assert_eq!(unit.applicability(), WorkUnitApplicability::Required);
            assert_eq!(snapshot.status(), ProgressStepStatus::Pending);
        }
    }

    #[test]
    fn cancellation_dispatch_manifest_excludes_completed_inapplicable_deferred_and_source_reads() {
        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let parent = NamespaceDirectoryEndpoint::new(
            source,
            source_id.clone(),
            NamespaceDirectoryLocator::Root,
            NamespaceDirectoryExpectation::new(7, "root-a", Some(1)).expect("root"),
        )
        .expect("parent");
        let args = UniversalCommandArgs::Namespace(NamespaceCommandArgs::CreateFolder(
            CreateFolderIntentArgs::new(parent, NamespaceLeafName::new("created").expect("leaf"))
                .expect("create-folder arguments"),
        ));
        let plan = super::super::work_plan::derive_work_plan(&args, &participants)
            .expect("create-folder plan");
        let mut progress = WorkProgress::from_plan_with_admission_observations(
            OperationId::from_uuid(Uuid::from_u128(318)),
            &plan,
            &args,
            &participants,
            &admission_capacity_plan(),
            admission_claims(12),
        )
        .expect("create-folder progress");
        apply_role(&mut progress, 0, WorkUnitRole::SourceRead);
        apply_role(&mut progress, 0, WorkUnitRole::Prepare);
        let stage = key_for(&progress, 0, WorkUnitRole::Stage);
        let copy_validated = key_for(&progress, 0, WorkUnitRole::CopyValidated);
        progress
            .request_cancellation(CancellationRequestObservation::new(318).expect("cancel"))
            .expect("create-folder cancellation");
        let manifest = progress
            .cancellation_dispatch_manifest()
            .expect("create-folder dispatch manifest");
        assert!(
            !manifest
                .keys()
                .contains(&key_for(&progress, 0, WorkUnitRole::SourceRead))
        );
        assert!(
            !manifest
                .keys()
                .contains(&key_for(&progress, 0, WorkUnitRole::Prepare))
        );
        assert!(!manifest.keys().contains(&stage));
        assert!(!manifest.keys().contains(&copy_validated));
        assert!(manifest.keys().iter().all(|key| {
            progress
                .plan()
                .units()
                .iter()
                .find(|unit| unit.key() == *key)
                .is_some_and(|unit| unit.applicability() == WorkUnitApplicability::Required)
        }));
        assert!(
            manifest
                .keys()
                .iter()
                .all(|key| key.role() != WorkUnitRole::SourceRead)
        );

        let mut optional = copy_progress_with_admission_claims_and_optional();
        complete_required(&mut optional);
        let artifact = optional
            .steps()
            .iter()
            .find(|step| step.requirement() == ParticipantRequirement::OptionalRebuildable)
            .map(WorkProgressStep::key)
            .expect("optional artifact");
        apply_readiness_deferral(&mut optional, &[artifact], 1);
        optional
            .request_cancellation(CancellationRequestObservation::new(319).expect("cancel"))
            .expect("deferred cancellation");
        assert!(
            optional
                .cancellation_dispatch_manifest()
                .expect("deferred dispatch manifest")
                .keys()
                .is_empty()
        );
    }

    #[test]
    fn cancellation_dispatch_manifest_validates_canonical_keys_and_survives_reacquisition() {
        let mut progress = copy_progress_with_admission_claims();
        progress
            .request_cancellation(CancellationRequestObservation::new(320).expect("cancel"))
            .expect("cancellation request");
        let manifest = progress
            .cancellation_dispatch_manifest()
            .expect("dispatch manifest");
        let inventory = progress
            .cancellation_inventory()
            .expect("cancellation inventory");
        let claims = progress.admission_claims().expect("admission claims");
        let mut missing = manifest.keys().to_vec();
        missing.remove(0);
        let missing = CancellationDispatchManifest::new(
            manifest.operation_id(),
            manifest.plan_version(),
            manifest.frozen_inventory_generation(),
            manifest.capture_epoch(),
            missing,
        )
        .expect("missing-key candidate shape");
        assert_eq!(
            missing.validate_against(
                progress.operation_id(),
                progress.plan(),
                inventory,
                progress.steps(),
                claims,
                progress.execution_epoch(),
            ),
            Err(CancellationDispatchManifestError::CandidateMismatch)
        );

        let mut extra_keys = manifest.keys().to_vec();
        extra_keys.push(key_for(&progress, 0, WorkUnitRole::SourceRead));
        extra_keys.sort();
        let extra = CancellationDispatchManifest::new(
            manifest.operation_id(),
            manifest.plan_version(),
            manifest.frozen_inventory_generation(),
            manifest.capture_epoch(),
            extra_keys,
        )
        .expect("extra-key candidate shape");
        assert_eq!(
            extra.validate_against(
                progress.operation_id(),
                progress.plan(),
                inventory,
                progress.steps(),
                claims,
                progress.execution_epoch(),
            ),
            Err(CancellationDispatchManifestError::CandidateMismatch)
        );

        let mut reordered_keys = manifest.keys().to_vec();
        reordered_keys.reverse();
        assert!(matches!(
            CancellationDispatchManifest::new(
                manifest.operation_id(),
                manifest.plan_version(),
                manifest.frozen_inventory_generation(),
                manifest.capture_epoch(),
                reordered_keys,
            ),
            Err(CancellationDispatchManifestValidationError::KeysNotSortedOrUnique { .. })
        ));

        let wrong_binding = CancellationDispatchManifest::new(
            OperationId::from_uuid(Uuid::from_u128(321)),
            manifest.plan_version(),
            manifest.frozen_inventory_generation(),
            manifest.capture_epoch(),
            manifest.keys().to_vec(),
        )
        .expect("wrong-binding candidate shape");
        assert_eq!(
            wrong_binding.validate_against(
                progress.operation_id(),
                progress.plan(),
                inventory,
                progress.steps(),
                claims,
                progress.execution_epoch(),
            ),
            Err(CancellationDispatchManifestError::OperationIdMismatch)
        );

        let frozen = manifest.clone();
        let target = key_for(&progress, 0, WorkUnitRole::SourceRead).participant_id();
        progress
            .reacquire_source(source_reacquisition_authority(&progress, target, 7, 2, 0))
            .expect("source reacquisition");
        assert_eq!(progress.execution_epoch(), 1);
        assert_eq!(
            progress
                .cancellation_dispatch_manifest()
                .expect("reacquired manifest"),
            frozen
        );
        frozen
            .validate_against(
                progress.operation_id(),
                progress.plan(),
                progress
                    .cancellation_inventory()
                    .expect("reacquired inventory"),
                progress.steps(),
                progress.admission_claims().expect("reacquired claims"),
                progress.execution_epoch(),
            )
            .expect("retained capture epoch remains valid");
        assert_eq!(frozen.capture_epoch(), 0);
    }

    #[test]
    fn cancellation_dispatch_outcome_record_requires_exact_manifest_coverage() {
        let mut progress = copy_progress_with_admission_claims();
        progress
            .request_cancellation(CancellationRequestObservation::new(322).expect("cancel"))
            .expect("cancellation request");
        let manifest = progress
            .cancellation_dispatch_manifest()
            .expect("dispatch manifest");
        let observed_key = key_for(&progress, 0, WorkUnitRole::Stage);
        assert!(manifest.keys().contains(&observed_key));
        let context = CompletionContext::new(
            progress.execution_epoch(),
            progress
                .scope_for(observed_key)
                .expect("stage scope")
                .clone(),
        );
        let record = CancellationDispatchOutcomeRecord::new(
            manifest.clone(),
            progress.execution_epoch(),
            1,
            dispatch_rows(
                &manifest,
                Some((
                    observed_key,
                    evidence_for(&progress, observed_key, 7),
                    context,
                )),
            ),
        )
        .expect("outcome record");
        let captured_later = CancellationDispatchManifest::new(
            manifest.operation_id(),
            manifest.plan_version(),
            manifest.frozen_inventory_generation(),
            manifest.capture_epoch() + 1,
            manifest.keys().to_vec(),
        )
        .expect("later-capture manifest shape");
        assert_eq!(
            CancellationDispatchOutcomeRecord::new(
                captured_later,
                record.observation_epoch(),
                record.recovery_generation(),
                dispatch_rows(&manifest, None),
            ),
            Err(
                CancellationDispatchOutcomeRecordError::ObservationEpochBeforeCapture {
                    actual: 0,
                    capture: 1,
                }
            )
        );
        let before = progress.clone();
        progress
            .validate_cancellation_dispatch_record(&record)
            .expect("exact outcome record");
        assert_eq!(progress, before);

        let mut missing_rows = record.rows().to_vec();
        missing_rows.pop();
        let missing = CancellationDispatchOutcomeRecord::new(
            manifest.clone(),
            record.observation_epoch(),
            record.recovery_generation() + 1,
            missing_rows,
        )
        .expect("missing-row candidate shape");
        assert_eq!(
            progress.validate_cancellation_dispatch_record(&missing),
            Err(WorkProgressError::CancellationDispatchOutcome(
                CancellationDispatchOutcomeRecordError::RowCountMismatch {
                    actual: manifest.keys().len() - 1,
                    expected: manifest.keys().len(),
                }
            ))
        );

        let source_key = key_for(&progress, 0, WorkUnitRole::SourceRead);
        let mut extra_rows = record.rows().to_vec();
        extra_rows.push(CancellationDispatchOutcomeRow::new(
            source_key,
            CancellationDispatchOutcome::EntryUncertain,
        ));
        extra_rows.sort_by_key(CancellationDispatchOutcomeRow::key);
        let extra = CancellationDispatchOutcomeRecord::new(
            manifest,
            record.observation_epoch(),
            record.recovery_generation() + 2,
            extra_rows,
        )
        .expect("extra-row candidate shape");
        assert!(matches!(
            progress.validate_cancellation_dispatch_record(&extra),
            Err(WorkProgressError::CancellationDispatchOutcome(
                CancellationDispatchOutcomeRecordError::RowCountMismatch { .. }
            ))
        ));
    }

    #[test]
    fn prepublish_observation_round_trips_dispatch_outcome_and_preserves_legacy_absence() {
        let mut progress = copy_progress_with_admission_claims();
        progress
            .request_cancellation(CancellationRequestObservation::new(3221).expect("cancel"))
            .expect("cancellation request");
        let manifest = progress
            .cancellation_dispatch_manifest()
            .expect("dispatch manifest");
        let observed_key = *manifest.keys().first().expect("dispatch key");
        let context = CompletionContext::new(
            progress.execution_epoch(),
            progress
                .scope_for(observed_key)
                .expect("observed scope")
                .clone(),
        );
        let record = CancellationDispatchOutcomeRecord::new(
            manifest,
            progress.execution_epoch(),
            1,
            dispatch_rows(
                &progress
                    .cancellation_dispatch_manifest()
                    .expect("dispatch manifest replay"),
                Some((
                    observed_key,
                    evidence_for(&progress, observed_key, 7),
                    context,
                )),
            ),
        )
        .expect("observed dispatch record");
        let observation = PrePublishCancellationObservation::with_dispatch_outcome(
            PrePublishCleanupObservation::new(Vec::new()).expect("cleanup"),
            PrePublishUnstartedQuiescenceObservation::new(Vec::new()).expect("quiescence"),
            settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Held),
            record,
        );
        let encoded = serde_json::to_value(&observation).expect("observation JSON");
        assert!(encoded.get("dispatch_outcome").is_some());
        let decoded: PrePublishCancellationObservation =
            serde_json::from_value(encoded).expect("observation JSON round trip");
        let decoded_record = decoded
            .dispatch_outcome()
            .expect("dispatch outcome retained after decode");
        progress
            .validate_cancellation_dispatch_record(decoded_record)
            .expect("decoded record validates before private use");
        assert_eq!(decoded, observation);

        let legacy = PrePublishCancellationObservation::new(
            PrePublishCleanupObservation::new(Vec::new()).expect("legacy cleanup"),
            settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Held),
        );
        let legacy_json = serde_json::to_value(&legacy).expect("legacy observation JSON");
        assert!(legacy_json.get("dispatch_outcome").is_none());
        let decoded_legacy: PrePublishCancellationObservation =
            serde_json::from_value(legacy_json).expect("legacy observation decodes");
        assert!(decoded_legacy.dispatch_outcome().is_none());
    }

    #[test]
    fn prepublish_dispatch_outcomes_keep_late_uncertainty_held_and_block_release() {
        let mut progress = copy_progress_with_admission_claims();
        progress
            .request_cancellation(CancellationRequestObservation::new(3222).expect("cancel"))
            .expect("cancellation request");
        let manifest = progress
            .cancellation_dispatch_manifest()
            .expect("dispatch manifest");
        let held = settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Held);

        let mut uncertain = pre_publish_cleanup_authority(
            &progress,
            PrePublishCleanupObservation::new(Vec::new()).expect("cleanup"),
            held.clone(),
        );
        uncertain.dispatch_outcome = CancellationDispatchOutcomeRecord::new(
            manifest.clone(),
            progress.execution_epoch(),
            1,
            dispatch_rows(&manifest, None),
        )
        .expect("uncertain dispatch record");
        progress
            .finish_pre_publish_cancellation(uncertain)
            .expect("retain uncertain dispatch observation");
        assert_eq!(progress.recovery_generation(), 1);
        assert!(
            progress
                .pre_publish_cancellation()
                .expect("retained cancellation")
                .dispatch_outcome()
                .expect("retained dispatch outcome")
                .rows()
                .iter()
                .all(|row| matches!(row.outcome(), CancellationDispatchOutcome::EntryUncertain))
        );

        let before_release = progress.clone();
        assert_eq!(
            progress.finish_pre_publish_cancellation(pre_publish_cleanup_authority(
                &progress,
                PrePublishCleanupObservation::new(Vec::new()).expect("cleanup"),
                settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Released),
            )),
            Err(WorkProgressError::PrePublishCancellationDispatchIncomplete)
        );
        assert_eq!(progress, before_release);

        let observed_key = key_for(&progress, 0, WorkUnitRole::Stage);
        let observed_context = CompletionContext::new(
            progress.execution_epoch(),
            progress
                .scope_for(observed_key)
                .expect("observed scope")
                .clone(),
        );
        let mut observed = pre_publish_cleanup_authority(
            &progress,
            PrePublishCleanupObservation::new(Vec::new()).expect("cleanup"),
            held,
        );
        observed.dispatch_outcome = CancellationDispatchOutcomeRecord::new(
            manifest.clone(),
            progress.execution_epoch(),
            2,
            dispatch_rows(
                &manifest,
                Some((
                    observed_key,
                    evidence_for(&progress, observed_key, 7),
                    observed_context,
                )),
            ),
        )
        .expect("observed dispatch record");
        progress
            .finish_pre_publish_cancellation(observed)
            .expect("retain late dispatch observation");
        assert_eq!(progress.recovery_generation(), 2);
        assert!(
            progress
                .pre_publish_cancellation()
                .expect("retained observed cancellation")
                .settlement()
                .settlements()
                .iter()
                .all(|row| row.outcome() == AdmissionClaimSettlementOutcome::Held)
        );

        let before_observed_release = progress.clone();
        assert_eq!(
            progress.finish_pre_publish_cancellation(pre_publish_cleanup_authority(
                &progress,
                PrePublishCleanupObservation::new(Vec::new()).expect("cleanup"),
                settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Released),
            )),
            Err(WorkProgressError::PrePublishCancellationDispatchIncomplete)
        );
        assert_eq!(progress, before_observed_release);

        let rewrite_context = CompletionContext::new(
            progress.execution_epoch(),
            progress
                .scope_for(observed_key)
                .expect("observed scope replay")
                .clone(),
        );
        let mut rewritten_observation = pre_publish_cleanup_authority(
            &progress,
            PrePublishCleanupObservation::new(Vec::new()).expect("cleanup"),
            settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Held),
        );
        rewritten_observation.dispatch_outcome = CancellationDispatchOutcomeRecord::new(
            progress
                .cancellation_dispatch_manifest()
                .expect("dispatch manifest replay"),
            progress.execution_epoch(),
            3,
            dispatch_rows(
                &progress
                    .cancellation_dispatch_manifest()
                    .expect("dispatch manifest replay"),
                Some((
                    observed_key,
                    StoredStepEvidence::Filesystem {
                        receipt: FilesystemReceiptSummary::new(FilesystemReceiptKind::Stage, 2),
                    },
                    rewrite_context,
                )),
            ),
        )
        .expect("rewritten late observation record");
        let before_rewrite = progress.clone();
        assert!(matches!(
            progress.finish_pre_publish_cancellation(rewritten_observation),
            Err(WorkProgressError::CancellationDispatchOutcome(
                CancellationDispatchOutcomeRecordError::LateObservationChanged { key }
            )) if key == observed_key
        ));
        assert_eq!(progress, before_rewrite);

        let mut retained_progress =
            copy_progress_with_claim_class(FullCapacityAllocationClass::JournalRecord, 3225);
        apply_role(&mut retained_progress, 0, WorkUnitRole::SourceRead);
        retained_progress
            .request_cancellation(CancellationRequestObservation::new(3225).expect("cancel"))
            .expect("retained cancellation request");
        let retained_manifest = retained_progress
            .cancellation_dispatch_manifest()
            .expect("retained dispatch manifest");
        let mut retained_uncertain = pre_publish_cleanup_authority(
            &retained_progress,
            PrePublishCleanupObservation::new(Vec::new()).expect("cleanup"),
            settlement_inventory(&retained_progress, AdmissionClaimSettlementOutcome::Held),
        );
        retained_uncertain.dispatch_outcome = CancellationDispatchOutcomeRecord::new(
            retained_manifest.clone(),
            retained_progress.execution_epoch(),
            1,
            dispatch_rows(&retained_manifest, None),
        )
        .expect("retained uncertain record");
        retained_progress
            .finish_pre_publish_cancellation(retained_uncertain)
            .expect("retain uncertain retained-charge observation");
        let before_consumed = retained_progress.clone();
        assert_eq!(
            retained_progress.finish_pre_publish_cancellation(pre_publish_cleanup_authority(
                &retained_progress,
                PrePublishCleanupObservation::new(Vec::new()).expect("cleanup"),
                settlement_inventory(
                    &retained_progress,
                    AdmissionClaimSettlementOutcome::ConsumedOrTransferred,
                ),
            )),
            Err(WorkProgressError::PrePublishCancellationDispatchIncomplete)
        );
        assert_eq!(retained_progress, before_consumed);

        let mut complete_dispatch = pre_publish_cleanup_authority(
            &retained_progress,
            PrePublishCleanupObservation::new(Vec::new()).expect("cleanup"),
            settlement_inventory(&retained_progress, AdmissionClaimSettlementOutcome::Held),
        );
        complete_dispatch.dispatch_outcome = all_never_dispatch_record(&retained_progress);
        retained_progress
            .finish_pre_publish_cancellation(complete_dispatch)
            .expect("retain complete dispatch observation");
        let before_quiescence_gate = retained_progress.clone();
        assert_eq!(
            retained_progress.finish_pre_publish_cancellation(pre_publish_cleanup_authority(
                &retained_progress,
                PrePublishCleanupObservation::new(Vec::new()).expect("cleanup"),
                settlement_inventory(
                    &retained_progress,
                    AdmissionClaimSettlementOutcome::ConsumedOrTransferred,
                ),
            )),
            Err(WorkProgressError::PrePublishCancellationSettlementGateIncomplete)
        );
        assert_eq!(retained_progress, before_quiescence_gate);
    }

    #[test]
    fn prepublish_dispatch_outcome_binding_and_refinement_fail_atomically() {
        let mut progress = copy_progress_with_admission_claims();
        progress
            .request_cancellation(CancellationRequestObservation::new(3223).expect("cancel"))
            .expect("cancellation request");
        let manifest = progress
            .cancellation_dispatch_manifest()
            .expect("dispatch manifest");
        let held = settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Held);

        let mut wrong_manifest_authority = pre_publish_cleanup_authority(
            &progress,
            PrePublishCleanupObservation::new(Vec::new()).expect("cleanup"),
            held.clone(),
        );
        let wrong_manifest = CancellationDispatchManifest::new(
            OperationId::from_uuid(Uuid::from_u128(3224)),
            manifest.plan_version(),
            manifest.frozen_inventory_generation(),
            manifest.capture_epoch(),
            manifest.keys().to_vec(),
        )
        .expect("wrong manifest shape");
        wrong_manifest_authority.dispatch_outcome = CancellationDispatchOutcomeRecord::new(
            wrong_manifest.clone(),
            progress.execution_epoch(),
            1,
            dispatch_rows(&wrong_manifest, None),
        )
        .expect("wrong manifest record");
        let before_wrong_manifest = progress.clone();
        assert!(matches!(
            progress.finish_pre_publish_cancellation(wrong_manifest_authority),
            Err(WorkProgressError::CancellationDispatchOutcome(
                CancellationDispatchOutcomeRecordError::ManifestMismatch
            ))
        ));
        assert_eq!(progress, before_wrong_manifest);

        let mut missing_rows_authority = pre_publish_cleanup_authority(
            &progress,
            PrePublishCleanupObservation::new(Vec::new()).expect("cleanup"),
            held.clone(),
        );
        let mut missing_rows = dispatch_rows(&manifest, None);
        missing_rows.pop();
        missing_rows_authority.dispatch_outcome = CancellationDispatchOutcomeRecord::new(
            manifest.clone(),
            progress.execution_epoch(),
            1,
            missing_rows,
        )
        .expect("missing rows record shape");
        let before_missing_rows = progress.clone();
        assert!(matches!(
            progress.finish_pre_publish_cancellation(missing_rows_authority),
            Err(WorkProgressError::CancellationDispatchOutcome(
                CancellationDispatchOutcomeRecordError::RowCountMismatch { .. }
            ))
        ));
        assert_eq!(progress, before_missing_rows);

        let mut wrong_generation_authority = pre_publish_cleanup_authority(
            &progress,
            PrePublishCleanupObservation::new(Vec::new()).expect("cleanup"),
            held.clone(),
        );
        wrong_generation_authority.dispatch_outcome = CancellationDispatchOutcomeRecord::new(
            manifest.clone(),
            progress.execution_epoch(),
            2,
            dispatch_rows(&manifest, None),
        )
        .expect("wrong generation record");
        let before_wrong_generation = progress.clone();
        assert_eq!(
            progress.finish_pre_publish_cancellation(wrong_generation_authority),
            Err(
                WorkProgressError::PrePublishCancellationDispatchGenerationMismatch {
                    expected: 1,
                    actual: 2,
                }
            )
        );
        assert_eq!(progress, before_wrong_generation);

        let mut accepted = pre_publish_cleanup_authority(
            &progress,
            PrePublishCleanupObservation::new(Vec::new()).expect("cleanup"),
            held,
        );
        accepted.dispatch_outcome = all_never_dispatch_record(&progress);
        progress
            .finish_pre_publish_cancellation(accepted)
            .expect("accept complete dispatch observation");
        let before_contradiction = progress.clone();
        let mut contradiction = pre_publish_cleanup_authority(
            &progress,
            PrePublishCleanupObservation::new(Vec::new()).expect("cleanup"),
            settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Held),
        );
        contradiction.dispatch_outcome = CancellationDispatchOutcomeRecord::new(
            manifest,
            progress.execution_epoch(),
            2,
            dispatch_rows(
                &progress
                    .cancellation_dispatch_manifest()
                    .expect("dispatch manifest replay"),
                None,
            ),
        )
        .expect("contradictory dispatch record");
        assert!(matches!(
            progress.finish_pre_publish_cancellation(contradiction),
            Err(WorkProgressError::CancellationDispatchOutcome(
                CancellationDispatchOutcomeRecordError::NeverEnteredChanged { .. }
            ))
        ));
        assert_eq!(progress, before_contradiction);
    }

    #[test]
    fn cancellation_dispatch_outcome_validates_late_history_and_reacquisition() {
        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let mut progress =
            claimed_progress(participants, history_folder_args(source, source_id), 323);
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::Prepare,
            WorkUnitRole::EnsureContainedParentDirectories,
            WorkUnitRole::QualifiedRelocation,
            WorkUnitRole::OriginAbsence,
            WorkUnitRole::SourceAffect,
        ] {
            apply_step(&mut progress, 0, 0, role);
        }
        progress
            .request_cancellation(CancellationRequestObservation::new(323).expect("cancel"))
            .expect("history cancellation");
        let manifest = progress
            .cancellation_dispatch_manifest()
            .expect("dispatch manifest");
        let late_key = key_for_step(&progress, 0, 1, WorkUnitRole::SourceAffect);
        assert!(manifest.keys().contains(&late_key));
        let historical_context = CompletionContext::new(
            progress.execution_epoch(),
            progress.scope_for(late_key).expect("source scope").clone(),
        );
        let historical_evidence = evidence_for(&progress, late_key, 7);
        let valid = CancellationDispatchOutcomeRecord::new(
            manifest.clone(),
            progress.execution_epoch(),
            1,
            dispatch_rows(
                &manifest,
                Some((
                    late_key,
                    historical_evidence.clone(),
                    historical_context.clone(),
                )),
            ),
        )
        .expect("historical outcome record");
        progress
            .validate_cancellation_dispatch_record(&valid)
            .expect("historical late completion");

        let before_steps = progress.steps().to_vec();
        let before_inventory = progress.cancellation_inventory().cloned();
        progress
            .reacquire_source(source_reacquisition_authority(
                &progress,
                late_key.participant_id(),
                7,
                2,
                0,
            ))
            .expect("source reacquisition");
        progress
            .validate_cancellation_dispatch_record(&valid)
            .expect("historical record after reacquisition");
        assert_eq!(progress.steps(), before_steps.as_slice());
        assert_eq!(progress.cancellation_inventory().cloned(), before_inventory);

        let wrong_role = CancellationDispatchOutcomeRecord::new(
            manifest.clone(),
            valid.observation_epoch(),
            2,
            dispatch_rows(
                &manifest,
                Some((
                    late_key,
                    StoredStepEvidence::Coordinator {
                        receipt: CoordinatorReceiptSummary::new(
                            CoordinatorReceiptKind::GlobalLibrary,
                            1,
                            None,
                        ),
                    },
                    historical_context.clone(),
                )),
            ),
        )
        .expect("wrong-role record shape");
        assert!(matches!(
            progress.validate_cancellation_dispatch_record(&wrong_role),
            Err(WorkProgressError::RoleEvidenceMismatch {
                role: WorkUnitRole::SourceAffect
            })
        ));

        let mut malformed_observation = source_observation(&progress, late_key, 7);
        malformed_observation.lease_epoch = 0;
        let malformed = CancellationDispatchOutcomeRecord::new(
            manifest.clone(),
            valid.observation_epoch(),
            2,
            dispatch_rows(
                &manifest,
                Some((
                    late_key,
                    StoredStepEvidence::SourceObserved {
                        observation: malformed_observation,
                    },
                    historical_context.clone(),
                )),
            ),
        )
        .expect_err("malformed nested evidence");
        assert!(matches!(
            malformed,
            CancellationDispatchOutcomeRecordError::Evidence { key, .. } if key == late_key
        ));

        let future_context = CompletionContext::new(
            progress.execution_epoch() + 1,
            progress
                .scope_for(late_key)
                .expect("current source scope")
                .clone(),
        );
        let future = CancellationDispatchOutcomeRecord::new(
            manifest,
            progress.execution_epoch() + 1,
            2,
            dispatch_rows(
                valid.manifest(),
                Some((late_key, historical_evidence, future_context)),
            ),
        )
        .expect("future-epoch record shape");
        assert!(matches!(
            progress.validate_cancellation_dispatch_record(&future),
            Err(WorkProgressError::CancellationDispatchOutcome(
                CancellationDispatchOutcomeRecordError::ObservationEpochFuture {
                    actual: 2,
                    current: 1,
                }
            ))
        ));
    }

    #[test]
    fn retained_late_dispatch_revisions_fence_source_reacquisition() {
        let mut progress = fenced_harvest_progress_with_admission_claims();
        progress
            .request_cancellation(CancellationRequestObservation::new(1308).expect("cancel"))
            .expect("cancellation request");
        let manifest = progress
            .cancellation_dispatch_manifest()
            .expect("dispatch manifest");
        let source_affect = key_for(&progress, 0, WorkUnitRole::SourceAffect);
        let harvest = key_for(&progress, u8::MAX, WorkUnitRole::Harvest);
        assert!(manifest.keys().contains(&source_affect));
        assert!(manifest.keys().contains(&harvest));
        let source_affect_context = CompletionContext::new(
            progress.execution_epoch(),
            progress
                .scope_for(source_affect)
                .expect("source-affect scope")
                .clone(),
        );
        let harvest_context = CompletionContext::new(
            progress.execution_epoch(),
            progress.scope_for(harvest).expect("harvest scope").clone(),
        );
        let record = CancellationDispatchOutcomeRecord::new(
            manifest.clone(),
            progress.execution_epoch(),
            1,
            dispatch_rows_with_observed(
                &manifest,
                &[
                    (
                        source_affect,
                        StoredStepEvidence::SourceCommitted {
                            before_revision: 8,
                            after: source_observation(&progress, source_affect, 8),
                        },
                        source_affect_context,
                    ),
                    (
                        harvest,
                        evidence_for(&progress, harvest, 9),
                        harvest_context,
                    ),
                ],
            ),
        )
        .expect("late dispatch record");
        let mut authority = pre_publish_cleanup_authority(
            &progress,
            PrePublishCleanupObservation::new(Vec::new()).expect("cleanup"),
            settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Held),
        );
        authority.dispatch_outcome = record.clone();
        progress
            .finish_pre_publish_cancellation(authority)
            .expect("retain late dispatch record");

        let before_rejected_reacquisition = progress.clone();
        assert_eq!(
            progress.reacquire_source(source_reacquisition_authority(
                &progress,
                source_affect.participant_id(),
                8,
                2,
                0,
            )),
            Err(WorkProgressError::SourceReacquisitionRevisionRegressed {
                observed: 8,
                minimum: 9,
            })
        );
        assert_eq!(progress, before_rejected_reacquisition);

        progress
            .reacquire_source(source_reacquisition_authority(
                &progress,
                source_affect.participant_id(),
                9,
                2,
                0,
            ))
            .expect("adequate source reacquisition");
        progress
            .validate_cancellation_dispatch_record(&record)
            .expect("retained late record after reacquisition");
        let recovery = progress
            .cancellation_effect_recovery_inventory(&record)
            .expect("recovery inventory after reacquisition");
        assert_eq!(recovery.dispatch_outcome(), &record);
        assert_eq!(progress.execution_epoch(), 1);
    }

    #[test]
    fn cancellation_dispatch_outcome_refinement_is_monotonic() {
        let mut progress = copy_progress_with_admission_claims();
        progress
            .request_cancellation(CancellationRequestObservation::new(324).expect("cancel"))
            .expect("cancellation request");
        let manifest = progress
            .cancellation_dispatch_manifest()
            .expect("dispatch manifest");
        let observed_key = key_for(&progress, 0, WorkUnitRole::Stage);
        let context = CompletionContext::new(
            progress.execution_epoch(),
            progress
                .scope_for(observed_key)
                .expect("stage scope")
                .clone(),
        );
        let uncertain = CancellationDispatchOutcomeRecord::new(
            manifest.clone(),
            0,
            1,
            dispatch_rows(&manifest, None),
        )
        .expect("uncertain record");
        assert!(uncertain.validate_transition(&uncertain, 0).is_ok());

        let observed = CancellationDispatchOutcomeRecord::new(
            manifest.clone(),
            0,
            2,
            dispatch_rows(
                &manifest,
                Some((
                    observed_key,
                    evidence_for(&progress, observed_key, 7),
                    context.clone(),
                )),
            ),
        )
        .expect("observed record");
        observed
            .validate_transition(&uncertain, 0)
            .expect("uncertain refinement");

        let same_generation = CancellationDispatchOutcomeRecord::new(
            manifest.clone(),
            0,
            1,
            observed.rows().to_vec(),
        )
        .expect("same-generation candidate");
        assert!(matches!(
            same_generation.validate_transition(&uncertain, 0),
            Err(
                CancellationDispatchOutcomeRecordError::RecoveryGenerationRegression {
                    previous: 1,
                    actual: 1,
                }
            )
        ));

        let epoch_previous = CancellationDispatchOutcomeRecord::new(
            manifest.clone(),
            1,
            1,
            dispatch_rows(&manifest, None),
        )
        .expect("later observation epoch");
        let epoch_regression = CancellationDispatchOutcomeRecord::new(
            manifest.clone(),
            0,
            2,
            dispatch_rows(&manifest, None),
        )
        .expect("regressed observation epoch");
        assert!(matches!(
            epoch_regression.validate_transition(&epoch_previous, 1),
            Err(
                CancellationDispatchOutcomeRecordError::ObservationEpochRegression {
                    previous: 1,
                    actual: 0,
                }
            )
        ));

        let never_entered = CancellationDispatchOutcomeRecord::new(
            manifest.clone(),
            0,
            2,
            manifest
                .keys()
                .iter()
                .map(|key| {
                    CancellationDispatchOutcomeRow::new(
                        *key,
                        CancellationDispatchOutcome::NeverEntered,
                    )
                })
                .collect(),
        )
        .expect("never-entered record");
        assert!(matches!(
            never_entered.validate_transition(&uncertain, 0),
            Ok(())
        ));
        let observed_after_never = CancellationDispatchOutcomeRecord::new(
            manifest.clone(),
            0,
            3,
            dispatch_rows(
                &manifest,
                Some((
                    observed_key,
                    evidence_for(&progress, observed_key, 7),
                    context.clone(),
                )),
            ),
        )
        .expect("observed after never-entered candidate");
        assert!(matches!(
            observed_after_never.validate_transition(&never_entered, 0),
            Err(CancellationDispatchOutcomeRecordError::NeverEnteredChanged { .. })
        ));

        let relabeled = CancellationDispatchOutcomeRecord::new(
            manifest.clone(),
            0,
            3,
            dispatch_rows(&manifest, None),
        )
        .expect("relabelled record");
        assert!(matches!(
            relabeled.validate_transition(&observed, 0),
            Err(CancellationDispatchOutcomeRecordError::LateObservationChanged { key })
                if key == observed_key
        ));

        let changed_evidence = CancellationDispatchOutcomeRecord::new(
            manifest.clone(),
            0,
            3,
            dispatch_rows(
                &manifest,
                Some((
                    observed_key,
                    StoredStepEvidence::Filesystem {
                        receipt: FilesystemReceiptSummary::new(FilesystemReceiptKind::Stage, 2),
                    },
                    context.clone(),
                )),
            ),
        )
        .expect("changed-evidence record");
        assert!(matches!(
            changed_evidence.validate_transition(&observed, 0),
            Err(CancellationDispatchOutcomeRecordError::LateObservationChanged { key })
                if key == observed_key
        ));

        let changed_context = CompletionContext::new(
            0,
            progress
                .scopes
                .iter()
                .find(|scope| scope.participant_id != observed_key.participant_id())
                .expect("different participant scope")
                .clone(),
        );
        let context_changed = CancellationDispatchOutcomeRecord::new(
            manifest.clone(),
            0,
            3,
            dispatch_rows(
                &manifest,
                Some((
                    observed_key,
                    evidence_for(&progress, observed_key, 7),
                    changed_context,
                )),
            ),
        )
        .expect("changed-context record");
        assert!(matches!(
            context_changed.validate_transition(&observed, 0),
            Err(CancellationDispatchOutcomeRecordError::LateObservationChanged { key })
                if key == observed_key
        ));
    }

    #[test]
    fn cancellation_inventory_classifies_nonpublication_effects_and_audit_hold() {
        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let mut purge = claimed_progress(
            participants,
            retained_resolution_args(source, source_id, false),
            208,
        );
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::Prepare,
            WorkUnitRole::SelectedRetainedObjectDisposition,
        ] {
            apply_role(&mut purge, 0, role);
        }
        purge
            .request_cancellation(CancellationRequestObservation::new(207).expect("cancel"))
            .expect("purge cancellation");
        let purge_inventory = purge
            .cancellation_inventory()
            .expect("purge cancellation inventory");
        assert_eq!(
            purge_inventory.actions()[0].state(),
            CancellationActionState::DestructiveDispositionWithoutPublish
        );
        assert_eq!(
            purge_inventory.route(),
            CancellationRoute::AfterObservedEffect
        );
        assert!(purge.phase().is_pre_publication());

        purge
            .require_audit(ProgressAuditObservation::new(
                ProgressAuditReason::UncertainEvidence,
            ))
            .expect("audit may cover the observed purge effect");
        assert_eq!(purge.disposition(), FullProgressDisposition::AuditRequired);
        purge
            .resolve_audit(audit_resolution_authority(&purge, 1, 0))
            .expect("resolve purge audit");
        assert_eq!(
            purge.disposition(),
            FullProgressDisposition::CancelRequestedAfterObservedEffect
        );

        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let mut merge = claimed_progress(
            participants,
            retained_resolution_args(source, source_id, true),
            209,
        );
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::Prepare,
            WorkUnitRole::RetainedMerge,
        ] {
            apply_role(&mut merge, 0, role);
        }
        merge
            .request_cancellation(CancellationRequestObservation::new(208).expect("cancel"))
            .expect("merge cancellation");
        let merge_inventory = merge
            .cancellation_inventory()
            .expect("merge cancellation inventory");
        assert_eq!(
            merge_inventory.actions()[0].state(),
            CancellationActionState::ApplicationMutationWithoutPublish
        );
        assert_eq!(
            merge_inventory.route(),
            CancellationRoute::AfterObservedEffect
        );
        assert_eq!(merge.phase(), OperationPhase::Prepared);

        let (participants, source, filesystem, source_id) = valid_topology(false);
        let mut external = claimed_progress(
            participants,
            external_reconciliation_args(source, filesystem, source_id),
            210,
        );
        apply_role(&mut external, 0, WorkUnitRole::SourceRead);
        apply_role(&mut external, 0, WorkUnitRole::ExternalAudit);
        external
            .request_cancellation(CancellationRequestObservation::new(209).expect("cancel"))
            .expect("external cancellation");
        let external_inventory = external
            .cancellation_inventory()
            .expect("external cancellation inventory");
        assert_eq!(
            external_inventory.actions()[0].state(),
            CancellationActionState::ExternalEffectAudited {
                source_reconciled: false
            }
        );
        assert_eq!(
            external_inventory.route(),
            CancellationRoute::AfterObservedEffect
        );
        assert!(external.phase().is_pre_publication());

        let (participants, source, filesystem, source_id) = valid_topology(false);
        let mut reconciled_external = claimed_progress(
            participants,
            external_reconciliation_args(source, filesystem, source_id),
            211,
        );
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::ExternalAudit,
            WorkUnitRole::SourceAffect,
        ] {
            apply_role(&mut reconciled_external, 0, role);
        }
        reconciled_external
            .request_cancellation(CancellationRequestObservation::new(210).expect("cancel"))
            .expect("reconciled external cancellation");
        assert_eq!(
            reconciled_external
                .cancellation_inventory()
                .expect("reconciled external inventory")
                .actions()[0]
                .state(),
            CancellationActionState::ExternalEffectAudited {
                source_reconciled: true
            }
        );
    }

    #[test]
    fn cancellation_replay_freezes_inventory_across_reacquisition() {
        let mut progress = copy_progress_with_admission_claims();
        let request = CancellationRequestObservation::new(211).expect("cancel observation");
        progress
            .request_cancellation(request)
            .expect("cancellation request");
        let frozen = progress
            .cancellation_inventory()
            .expect("frozen cancellation inventory")
            .clone();
        let target = key_for(&progress, 0, WorkUnitRole::SourceRead).participant_id();

        progress
            .reacquire_source(source_reacquisition_authority(&progress, target, 7, 2, 0))
            .expect("source reacquisition");
        assert_eq!(progress.execution_epoch(), 1);
        assert_eq!(progress.cancellation_inventory(), Some(&frozen));
        frozen
            .validate_against(
                progress.operation_id(),
                progress.plan(),
                progress.steps(),
                progress.execution_epoch(),
                progress.admission_claims().expect("admission claims"),
            )
            .expect("capture epoch remains valid after reacquisition");
        let mut future_epoch = serde_json::to_value(&frozen).expect("inventory JSON");
        future_epoch["execution_epoch"] = serde_json::json!(2);
        let future_epoch: CancellationInventory =
            serde_json::from_value(future_epoch).expect("future epoch shape decodes");
        assert!(matches!(
            future_epoch.validate_against(
                progress.operation_id(),
                progress.plan(),
                progress.steps(),
                progress.execution_epoch(),
                progress.admission_claims().expect("admission claims"),
            ),
            Err(CancellationInventoryError::ExecutionEpochFuture {
                current: 1,
                actual: 2
            })
        ));
        progress
            .request_cancellation(request)
            .expect("exact cancellation replay");
        assert_eq!(progress.cancellation_inventory(), Some(&frozen));

        let before_conflict = progress.clone();
        assert_eq!(
            progress.request_cancellation(CancellationRequestObservation::new(212).unwrap()),
            Err(WorkProgressError::CancellationReplayConflict)
        );
        assert_eq!(progress, before_conflict);
    }

    #[test]
    fn cancellation_inventory_validates_prerequisites_and_bounded_keys() {
        let mut progress = copy_progress_with_admission_claims();
        progress
            .request_cancellation(CancellationRequestObservation::new(213).expect("cancel"))
            .expect("cancellation request");
        let inventory = progress
            .cancellation_inventory()
            .expect("cancellation inventory")
            .clone();
        let claims = progress.admission_claims().expect("admission claims");

        let mut unknown_key = serde_json::to_value(&inventory).expect("inventory JSON");
        unknown_key["steps"][0]["key"]["step_ordinal"] = serde_json::json!(999);
        let unknown_key: CancellationInventory =
            serde_json::from_value(unknown_key).expect("unknown key shape decodes");
        assert!(matches!(
            unknown_key.validate_against(
                progress.operation_id(),
                progress.plan(),
                progress.steps(),
                progress.execution_epoch(),
                claims,
            ),
            Err(CancellationInventoryError::StepKeyMismatch { index: 0, .. })
        ));

        let mut oversized = serde_json::to_value(&inventory).expect("inventory JSON");
        let first_step = oversized["steps"][0].clone();
        let steps = oversized["steps"].as_array_mut().expect("step array");
        for _ in 0..MAX_CANCELLATION_STEPS {
            steps.push(first_step.clone());
        }
        let oversized: CancellationInventory =
            serde_json::from_value(oversized).expect("oversized inventory decodes");
        assert!(matches!(
            oversized.validate_against(
                progress.operation_id(),
                progress.plan(),
                progress.steps(),
                progress.execution_epoch(),
                claims,
            ),
            Err(CancellationInventoryError::TooManySteps { .. })
        ));

        let mut tampered_publish = serde_json::to_value(&inventory).expect("inventory JSON");
        let publish_roles = [
            WorkUnitRole::SourceRead,
            WorkUnitRole::Prepare,
            WorkUnitRole::Stage,
            WorkUnitRole::CopyValidated,
            WorkUnitRole::Publish,
        ];
        let steps = tampered_publish["steps"]
            .as_array_mut()
            .expect("step array");
        for (index, unit) in progress.plan().units().iter().enumerate() {
            if unit.key().action_ordinal() == 0 && publish_roles.contains(&unit.key().role()) {
                steps[index]["status"] = serde_json::json!("Completed");
            }
        }
        tampered_publish["actions"][0]["state"] = serde_json::json!("AppPublished");
        tampered_publish["route"] = serde_json::json!("AfterPublish");
        let tampered_publish: CancellationInventory =
            serde_json::from_value(tampered_publish).expect("valid publish snapshot shape");
        assert!(matches!(
            tampered_publish.validate_against(
                progress.operation_id(),
                progress.plan(),
                progress.steps(),
                progress.execution_epoch(),
                claims,
            ),
            Err(CancellationInventoryError::CurrentProgressMismatch { .. })
        ));

        let mut invalid_progress = copy_progress_with_admission_claims();
        let dependent = key_for(&invalid_progress, 0, WorkUnitRole::Prepare);
        let dependent_index = invalid_progress
            .steps
            .iter()
            .position(|step| step.key() == dependent)
            .expect("dependent step");
        invalid_progress.steps[dependent_index].status = ProgressStepStatus::Completed;
        let error = CancellationInventory::capture(
            invalid_progress.operation_id,
            &invalid_progress.plan,
            invalid_progress.execution_epoch,
            1,
            &invalid_progress.steps,
            invalid_progress
                .admission_claims
                .as_ref()
                .expect("admission claims"),
        )
        .expect_err("incomplete prerequisite");
        assert!(matches!(
            error,
            CancellationInventoryError::PrerequisiteIncomplete { .. }
        ));
    }

    #[test]
    fn required_completion_and_optional_artifact_deferral_have_distinct_terminals() {
        let mut progress = copy_progress(true);
        complete_required(&mut progress);
        assert_eq!(progress.phase(), OperationPhase::ReadinessScheduled);
        assert_eq!(progress.disposition(), FullProgressDisposition::None);
        let artifact_key = progress
            .steps()
            .iter()
            .find(|step| step.requirement() == ParticipantRequirement::OptionalRebuildable)
            .map(WorkProgressStep::key)
            .expect("optional artifact");
        apply_readiness_deferral(&mut progress, &[artifact_key], 1);
        assert_eq!(progress.phase(), OperationPhase::ReadinessScheduled);
        assert_eq!(progress.disposition(), FullProgressDisposition::None);

        let mut required_only = copy_progress_with_admission_claims();
        complete_required(&mut required_only);
        let settlement =
            settlement_inventory(&required_only, AdmissionClaimSettlementOutcome::Released);
        required_only
            .apply_verified_claim_settlement(settlement_authority(&required_only, settlement))
            .expect("claim settlement applies");
        assert_eq!(required_only.phase(), OperationPhase::Terminal);
        assert_eq!(
            required_only.disposition(),
            FullProgressDisposition::Succeeded
        );

        let mut optional = copy_progress_with_admission_claims_and_optional();
        complete_required(&mut optional);
        let before_settlement = optional.clone();
        let settlement = settlement_inventory(&optional, AdmissionClaimSettlementOutcome::Released);
        assert_eq!(
            optional.apply_verified_claim_settlement(settlement_authority(&optional, settlement)),
            Err(WorkProgressError::ClaimSettlementProgressIncomplete)
        );
        assert_eq!(optional, before_settlement);
        let optional_artifact = optional
            .steps()
            .iter()
            .find(|step| step.requirement() == ParticipantRequirement::OptionalRebuildable)
            .map(WorkProgressStep::key)
            .expect("optional artifact");
        apply_readiness_deferral(&mut optional, &[optional_artifact], 1);
        let settlement = settlement_inventory(&optional, AdmissionClaimSettlementOutcome::Released);
        optional
            .apply_verified_claim_settlement(settlement_authority(&optional, settlement))
            .expect("deferred claim settlement applies");
        assert_eq!(optional.phase(), OperationPhase::Terminal);
        assert_eq!(
            optional.disposition(),
            FullProgressDisposition::SucceededWithDeferredArtifacts
        );
    }

    #[test]
    fn readiness_deferral_observation_binds_all_prerequisites_without_mutation() {
        let progress = copy_progress(true);
        let artifact = progress
            .steps()
            .iter()
            .find(|step| step.requirement() == ParticipantRequirement::OptionalRebuildable)
            .map(WorkProgressStep::key)
            .expect("optional Artifact");
        let before = progress.clone();
        let manifest = progress
            .readiness_deferral_manifest(&[artifact])
            .expect("readiness manifest");
        assert_eq!(manifest.artifact_keys(), &[artifact]);
        assert!(!manifest.readiness_keys().is_empty());
        assert!(manifest.readiness_keys().iter().all(|key| {
            progress
                .plan()
                .prerequisites()
                .iter()
                .any(|edge| edge.prerequisite() == *key && edge.dependent() == artifact)
        }));

        let rows = manifest
            .readiness_keys()
            .iter()
            .enumerate()
            .map(|(index, key)| readiness_row(&progress, *key, 700 + index as u128))
            .collect();
        let record = ReadinessSchedulingObservationRecord::new(
            manifest.clone(),
            progress.execution_epoch(),
            1,
            rows,
        )
        .expect("readiness observation record");
        progress
            .validate_readiness_scheduling_observation_record(&record, &[artifact])
            .expect("current readiness observation");
        assert_eq!(progress, before);
        assert_eq!(
            progress
                .step_for(artifact)
                .expect("optional Artifact step")
                .status(),
            ProgressStepStatus::Pending
        );
    }

    #[test]
    fn readiness_deferral_accepts_exact_subsets_replays_and_keeps_history() {
        let mut progress = cross_source_optional_progress();
        complete_required_at(&mut progress, 8);
        let artifacts = progress
            .steps()
            .iter()
            .filter(|step| step.requirement() == ParticipantRequirement::OptionalRebuildable)
            .map(WorkProgressStep::key)
            .collect::<Vec<_>>();
        assert_eq!(artifacts.len(), 2);

        let first_observation = readiness_deferral_observation(&progress, &[artifacts[0]], 1);
        let first_authority = readiness_deferral_authority(&progress, first_observation.clone(), 0);
        assert_eq!(
            progress.apply_verified_readiness_deferral(first_authority),
            Ok(ProgressApplyOutcome::Applied)
        );
        assert_eq!(
            progress
                .step_for(artifacts[0])
                .expect("first artifact")
                .status(),
            ProgressStepStatus::Deferred
        );
        assert_eq!(
            progress
                .step_for(artifacts[1])
                .expect("second artifact")
                .status(),
            ProgressStepStatus::Pending
        );

        let replay = readiness_deferral_authority(&progress, first_observation, 0);
        assert_eq!(
            progress.apply_verified_readiness_deferral(replay),
            Ok(ProgressApplyOutcome::Replay)
        );
        assert_eq!(progress.readiness_deferral_batches().len(), 1);

        let second_observation = readiness_deferral_observation(&progress, &[artifacts[1]], 2);
        assert_eq!(
            progress.apply_verified_readiness_deferral(readiness_deferral_authority(
                &progress,
                second_observation,
                1,
            )),
            Ok(ProgressApplyOutcome::Applied)
        );
        assert_eq!(progress.readiness_deferral_batches().len(), 2);
        assert!(
            progress
                .steps()
                .iter()
                .filter(|step| step.requirement() == ParticipantRequirement::OptionalRebuildable)
                .all(|step| step.status() == ProgressStepStatus::Deferred)
        );
        let before_overlap = progress.clone();
        let overlap = readiness_deferral_observation(&progress, &[artifacts[0]], 3);
        assert!(matches!(
            progress.apply_verified_readiness_deferral(readiness_deferral_authority(
                &progress,
                overlap,
                2,
            )),
            Err(WorkProgressError::ReadinessDeferralArtifactNotPending { key })
                if key == artifacts[0]
        ));
        assert_eq!(progress, before_overlap);
        progress
            .validate_completion_history()
            .expect("retained deferral history");
    }

    #[test]
    fn readiness_deferral_rejects_same_generation_payload_conflict_atomically() {
        let mut progress = copy_progress(true);
        complete_required(&mut progress);
        let artifact = progress
            .steps()
            .iter()
            .find(|step| step.requirement() == ParticipantRequirement::OptionalRebuildable)
            .map(WorkProgressStep::key)
            .expect("optional artifact");
        let observation = readiness_deferral_observation(&progress, &[artifact], 1);
        progress
            .apply_verified_readiness_deferral(readiness_deferral_authority(
                &progress,
                observation.clone(),
                0,
            ))
            .expect("initial deferral");
        let before_wrong_previous = progress.clone();
        assert!(matches!(
            progress.apply_verified_readiness_deferral(readiness_deferral_authority(
                &progress,
                observation.clone(),
                1,
            )),
            Err(WorkProgressError::ReadinessDeferralGenerationMismatch {
                expected: 2,
                actual: 1,
            })
        ));
        assert_eq!(progress, before_wrong_previous);

        let mut wrong_manifest = serde_json::to_value(&observation).expect("observation JSON");
        wrong_manifest["manifest"]["operation_id"] =
            serde_json::to_value(OperationId::from_uuid(Uuid::from_u128(9999)))
                .expect("operation ID JSON");
        let wrong_manifest: ReadinessSchedulingObservationRecord =
            serde_json::from_value(wrong_manifest).expect("wrong-manifest observation shape");
        let before_wrong_manifest = progress.clone();
        assert!(matches!(
            progress.apply_verified_readiness_deferral(readiness_deferral_authority(
                &progress,
                wrong_manifest,
                0,
            )),
            Err(WorkProgressError::ReadinessSchedulingObservation(
                ReadinessSchedulingObservationError::Manifest(
                    ReadinessDeferralManifestError::ExpectedManifestMismatch
                )
            ))
        ));
        assert_eq!(progress, before_wrong_manifest);

        let mut conflicting = serde_json::to_value(&observation).expect("observation JSON");
        conflicting["rows"][0]["wake"]["owner_revision"] = serde_json::json!(2);
        let conflicting: ReadinessSchedulingObservationRecord =
            serde_json::from_value(conflicting).expect("conflicting observation shape");
        let before = progress.clone();
        assert!(matches!(
            progress.apply_verified_readiness_deferral(readiness_deferral_authority(
                &progress,
                conflicting,
                0,
            )),
            Err(WorkProgressError::ReadinessDeferralPublicationConflict { .. })
        ));
        assert_eq!(progress, before);
    }

    #[test]
    fn readiness_deferral_source_floor_and_snapshot_history_are_preserved() {
        let mut progress = copy_progress_with_admission_claims_and_optional();
        complete_required(&mut progress);
        let artifact = progress
            .steps()
            .iter()
            .find(|step| step.requirement() == ParticipantRequirement::OptionalRebuildable)
            .map(WorkProgressStep::key)
            .expect("optional artifact");
        let observation = readiness_deferral_observation(&progress, &[artifact], 1);
        let mut elevated = serde_json::to_value(&observation).expect("observation JSON");
        elevated["rows"][0]["source_observation"]["revision"] = serde_json::json!(8);
        let elevated: ReadinessSchedulingObservationRecord =
            serde_json::from_value(elevated).expect("elevated observation");
        progress
            .apply_verified_readiness_deferral(readiness_deferral_authority(
                &progress,
                elevated.clone(),
                0,
            ))
            .expect("elevated deferral");

        let readiness_key = progress
            .readiness_deferral_batches()
            .first()
            .expect("accepted batch")
            .rows()[0]
            .readiness_key();
        let readiness_fence = progress
            .scope_for(readiness_key)
            .and_then(ParticipantScope::source_fence)
            .expect("readiness fence");
        let source_target = progress
            .scopes
            .iter()
            .find(|scope| {
                scope.kind() == ParticipantKind::SourceDatabase
                    && scope
                        .source_fence()
                        .is_some_and(|fence| same_stable_source_identity(fence, readiness_fence))
            })
            .map(ParticipantScope::participant_id)
            .expect("matching source database");
        let before_reacquisition = progress.clone();
        assert_eq!(
            progress.reacquire_source(source_reacquisition_authority(
                &progress,
                source_target,
                7,
                2,
                0,
            )),
            Err(WorkProgressError::SourceReacquisitionRevisionRegressed {
                observed: 7,
                minimum: 8,
            })
        );
        assert_eq!(progress, before_reacquisition);

        let (participants, source, _filesystem, source_id) = valid_topology(true);
        let snapshot = progress
            .capture_full_progress_snapshot(&snapshot_intent_for(
                &progress,
                copy_args_at(source, source_id, "copy.wav"),
                participants,
            ))
            .expect("deferral snapshot");
        let mut regressed = serde_json::to_value(&snapshot).expect("snapshot JSON");
        regressed["readiness_deferral_batches"][0]["rows"][0]["source_observation"]["revision"] =
            serde_json::json!(6);
        let regressed: FullProgressSnapshotObservation =
            serde_json::from_value(regressed).expect("regressed snapshot shape");
        assert!(matches!(
            regressed.validate_base(),
            Err(
                FullProgressSnapshotObservationError::ReadinessDeferralValidation {
                    source: WorkProgressError::ReadinessDeferralSourceRevisionRegression {
                        observed: 6,
                        minimum: 7,
                        ..
                    }
                }
            )
        ));

        let mut publication_regressed = serde_json::to_value(&snapshot).expect("snapshot JSON");
        publication_regressed["readiness_deferral_batches"][0]["rows"][0]["readiness_revision"] =
            serde_json::json!(0);
        let publication_regressed: FullProgressSnapshotObservation =
            serde_json::from_value(publication_regressed).expect("publication regression shape");
        assert!(matches!(
            publication_regressed.validate_base(),
            Err(
                FullProgressSnapshotObservationError::ReadinessDeferralValidation {
                    source: WorkProgressError::ReadinessDeferralRevisionRegression { .. }
                }
            )
        ));

        let mut source_generation_regressed =
            serde_json::to_value(&snapshot).expect("snapshot JSON");
        source_generation_regressed["readiness_deferral_batches"][0]["rows"][0]["source_generation"] =
            serde_json::json!(0);
        let source_generation_regressed: FullProgressSnapshotObservation =
            serde_json::from_value(source_generation_regressed)
                .expect("source-generation regression shape");
        assert!(matches!(
            source_generation_regressed.validate_base(),
            Err(
                FullProgressSnapshotObservationError::ReadinessDeferralValidation {
                    source: WorkProgressError::ReadinessDeferralSourceGenerationRegression { .. }
                }
            )
        ));

        let mut metadata_conflict = serde_json::to_value(&snapshot).expect("snapshot JSON");
        metadata_conflict["readiness_deferral_batches"][0]["rows"][0]["contract_version"] =
            serde_json::json!("different-contract-v1");
        let metadata_conflict: FullProgressSnapshotObservation =
            serde_json::from_value(metadata_conflict).expect("metadata conflict shape");
        assert!(matches!(
            metadata_conflict.validate_base(),
            Err(
                FullProgressSnapshotObservationError::ReadinessDeferralValidation {
                    source: WorkProgressError::ReadinessDeferralPublicationConflict { .. }
                }
            )
        ));

        let stale_authority = readiness_deferral_authority(&progress, elevated.clone(), 0);
        progress
            .reacquire_source(source_reacquisition_authority(
                &progress,
                source_target,
                8,
                2,
                0,
            ))
            .expect("valid source reacquisition retaining deferral receipt");
        progress
            .validate_completion_history()
            .expect("historical deferral survives reacquisition");
        let (participants, source, _filesystem, source_id) = valid_topology(true);
        let rebound_snapshot = progress
            .capture_full_progress_snapshot(&snapshot_intent_for(
                &progress,
                copy_args_at(source, source_id, "copy.wav"),
                participants,
            ))
            .expect("rebound deferral snapshot");
        rebound_snapshot
            .validate_base()
            .expect("rebound deferral snapshot validates");
        let before_stale_authority = progress.clone();
        assert!(matches!(
            progress.apply_verified_readiness_deferral(stale_authority),
            Err(WorkProgressError::ReadinessDeferralEpochMismatch {
                expected: 1,
                actual: 0,
            })
        ));
        assert_eq!(progress, before_stale_authority);
    }

    #[test]
    fn readiness_deferral_rejects_incomplete_readiness_atomically() {
        let mut progress = copy_progress(true);
        let artifact = progress
            .steps()
            .iter()
            .find(|step| step.requirement() == ParticipantRequirement::OptionalRebuildable)
            .map(WorkProgressStep::key)
            .expect("optional artifact");
        let manifest = progress
            .readiness_deferral_manifest(&[artifact])
            .expect("readiness manifest");
        let rows = manifest
            .readiness_keys()
            .iter()
            .map(|key| readiness_row(&progress, *key, 900))
            .collect();
        let observation = ReadinessSchedulingObservationRecord::new(
            manifest,
            progress.execution_epoch(),
            1,
            rows,
        )
        .expect("incomplete-readiness observation shape");
        let before = progress.clone();
        assert!(matches!(
            progress.apply_verified_readiness_deferral(readiness_deferral_authority(
                &progress,
                observation,
                0,
            )),
            Err(WorkProgressError::ReadinessDeferralReadinessIncomplete { .. })
        ));
        assert_eq!(progress, before);
    }

    #[test]
    fn snapshot_rejects_overcounted_readiness_deferral_history_before_reconstruction() {
        let mut progress = copy_progress_with_admission_claims_and_optional();
        complete_required(&mut progress);
        let artifact = progress
            .steps()
            .iter()
            .find(|step| step.requirement() == ParticipantRequirement::OptionalRebuildable)
            .map(WorkProgressStep::key)
            .expect("optional artifact");
        apply_readiness_deferral(&mut progress, &[artifact], 1);
        let (participants, source, _filesystem, source_id) = valid_topology(true);
        let snapshot = progress
            .capture_full_progress_snapshot(&snapshot_intent_for(
                &progress,
                copy_args_at(source, source_id, "copy.wav"),
                participants,
            ))
            .expect("deferral snapshot");
        let mut overcounted = serde_json::to_value(&snapshot).expect("snapshot JSON");
        let first = overcounted["readiness_deferral_batches"][0].clone();
        let batches = overcounted["readiness_deferral_batches"]
            .as_array_mut()
            .expect("batch array");
        for _ in 0..snapshot.intent().plan().units().len() {
            batches.push(first.clone());
        }
        let overcounted: FullProgressSnapshotObservation =
            serde_json::from_value(overcounted).expect("overcounted snapshot shape");
        assert!(matches!(
            overcounted.validate_base(),
            Err(FullProgressSnapshotObservationError::TooManyReadinessDeferralBatches { .. })
                | Err(
                    FullProgressSnapshotObservationError::TooManyReadinessDeferralObservations { .. }
                )
        ));

        let mut missing_proof = serde_json::to_value(&snapshot).expect("snapshot JSON");
        missing_proof["readiness_deferral_batches"] = serde_json::json!([]);
        let missing_proof: FullProgressSnapshotObservation =
            serde_json::from_value(missing_proof).expect("missing-proof snapshot shape");
        assert!(matches!(
            missing_proof.validate_base(),
            Err(
                FullProgressSnapshotObservationError::ReadinessDeferralValidation {
                    source: WorkProgressError::ReadinessDeferralStatusCoverageMismatch
                }
            )
        ));

        let mut wrong_generation = serde_json::to_value(&snapshot).expect("snapshot JSON");
        wrong_generation["readiness_deferral_batches"][0]["observation_generation"] =
            serde_json::json!(2);
        let wrong_generation: FullProgressSnapshotObservation =
            serde_json::from_value(wrong_generation).expect("wrong-generation snapshot shape");
        assert!(matches!(
            wrong_generation.validate_base(),
            Err(
                FullProgressSnapshotObservationError::ReadinessDeferralValidation {
                    source: WorkProgressError::ReadinessDeferralGenerationMismatch {
                        expected: 1,
                        actual: 2
                    }
                }
            )
        ));
    }

    #[test]
    fn compact_deferral_batch_rejects_duplicate_wakes() {
        let mut progress = cross_source_optional_progress();
        complete_required_at(&mut progress, 8);
        let artifacts = progress
            .steps()
            .iter()
            .filter(|step| step.requirement() == ParticipantRequirement::OptionalRebuildable)
            .map(WorkProgressStep::key)
            .collect::<Vec<_>>();
        let manifest = progress
            .readiness_deferral_manifest(&artifacts)
            .expect("cross-source manifest");
        let rows = manifest
            .readiness_keys()
            .iter()
            .map(|key| {
                progress
                    .step_for(*key)
                    .and_then(WorkProgressStep::evidence)
                    .and_then(|evidence| match evidence {
                        StoredStepEvidence::ReadinessScheduled { observation } => {
                            Some(observation.clone())
                        }
                        _ => None,
                    })
                    .expect("rich readiness evidence")
            })
            .collect::<Vec<_>>();
        assert!(rows.len() >= 2);
        let mut duplicate_rows = rows;
        let first_wake = duplicate_rows[0].wake().clone();
        duplicate_rows[1] = ReadinessSchedulingObservationRow::new(
            duplicate_rows[1].readiness_key(),
            duplicate_rows[1].context().clone(),
            duplicate_rows[1].source_observation().clone(),
            duplicate_rows[1].source_generation(),
            duplicate_rows[1].readiness_revision(),
            duplicate_rows[1].contract_version(),
            *duplicate_rows[1].target_set(),
            first_wake,
        )
        .expect("duplicate wake row");
        assert!(matches!(
            ReadinessDeferralBatchObservation::new(
                1,
                progress.execution_epoch(),
                artifacts,
                duplicate_rows
            ),
            Err(ReadinessSchedulingObservationError::DuplicateWakeId { .. })
        ));
    }

    #[test]
    fn readiness_manifest_binds_each_artifact_to_its_same_source_readiness() {
        let progress = cross_source_optional_progress();
        let artifacts = progress
            .steps()
            .iter()
            .filter(|step| step.requirement() == ParticipantRequirement::OptionalRebuildable)
            .map(WorkProgressStep::key)
            .collect::<Vec<_>>();
        assert_eq!(artifacts.len(), 2);
        let before = progress.clone();
        let manifest = progress
            .readiness_deferral_manifest(&artifacts)
            .expect("cross-source readiness manifest");
        assert_eq!(manifest.bindings().len(), 2);
        assert_eq!(manifest.readiness_keys().len(), 2);
        for binding in manifest.bindings() {
            let artifact_scope = progress
                .scope_for(binding.artifact_key())
                .expect("Artifact scope");
            let readiness_scope = progress
                .scope_for(binding.readiness_key())
                .expect("Readiness scope");
            assert_eq!(
                artifact_scope.source_fence(),
                readiness_scope.source_fence()
            );
        }
        let rows = manifest
            .readiness_keys()
            .iter()
            .enumerate()
            .map(|(index, key)| readiness_row(&progress, *key, 750 + index as u128))
            .collect();
        let record = ReadinessSchedulingObservationRecord::new(
            manifest.clone(),
            progress.execution_epoch(),
            1,
            rows,
        )
        .expect("cross-source record");
        progress
            .validate_readiness_scheduling_observation_record(&record, &artifacts)
            .expect("cross-source readiness record");
        assert_eq!(progress, before);

        let mut wrong_binding = serde_json::to_value(&manifest).expect("manifest JSON");
        wrong_binding["bindings"][0]["readiness_key"] =
            serde_json::to_value(manifest.bindings()[1].readiness_key())
                .expect("readiness key JSON");
        wrong_binding["bindings"][1]["readiness_key"] =
            serde_json::to_value(manifest.bindings()[0].readiness_key())
                .expect("readiness key JSON");
        let wrong_binding: ReadinessDeferralManifest =
            serde_json::from_value(wrong_binding).expect("wrong binding shape");
        let wrong_binding_rows = wrong_binding
            .readiness_keys()
            .iter()
            .enumerate()
            .map(|(index, key)| readiness_row(&progress, *key, 760 + index as u128))
            .collect();
        let wrong_binding_record = ReadinessSchedulingObservationRecord::new(
            wrong_binding,
            progress.execution_epoch(),
            1,
            wrong_binding_rows,
        )
        .expect("wrong binding record shape");
        assert!(matches!(
            progress.validate_readiness_scheduling_observation_record(
                &wrong_binding_record,
                &artifacts,
            ),
            Err(WorkProgressError::ReadinessSchedulingObservation(
                ReadinessSchedulingObservationError::Manifest(
                    ReadinessDeferralManifestError::ExpectedManifestMismatch
                )
            ))
        ));

        let duplicate_wake_rows = manifest
            .readiness_keys()
            .iter()
            .map(|key| readiness_row(&progress, *key, 761))
            .collect();
        assert!(matches!(
            ReadinessSchedulingObservationRecord::new(
                manifest.clone(),
                progress.execution_epoch(),
                1,
                duplicate_wake_rows,
            ),
            Err(ReadinessSchedulingObservationError::DuplicateWakeId { .. })
        ));

        let mut reordered_rows = manifest
            .readiness_keys()
            .iter()
            .enumerate()
            .map(|(index, key)| readiness_row(&progress, *key, 770 + index as u128))
            .collect::<Vec<_>>();
        reordered_rows.reverse();
        assert!(matches!(
            ReadinessSchedulingObservationRecord::new(
                manifest,
                progress.execution_epoch(),
                1,
                reordered_rows,
            ),
            Err(ReadinessSchedulingObservationError::RowsNotSortedUnique)
        ));
    }

    #[test]
    fn readiness_deferral_observation_rejects_selection_and_shape_errors() {
        let progress = copy_progress(true);
        let artifact = progress
            .steps()
            .iter()
            .find(|step| step.requirement() == ParticipantRequirement::OptionalRebuildable)
            .map(WorkProgressStep::key)
            .expect("optional Artifact");
        let readiness = progress
            .steps()
            .iter()
            .find(|step| step.key().role() == WorkUnitRole::Readiness)
            .map(WorkProgressStep::key)
            .expect("Readiness");
        assert!(matches!(
            progress.readiness_deferral_manifest(&[]),
            Err(WorkProgressError::ReadinessDeferralManifest(
                ReadinessDeferralManifestError::EmptyArtifactSelection
            ))
        ));
        assert!(matches!(
            progress.readiness_deferral_manifest(&[readiness]),
            Err(WorkProgressError::ReadinessDeferralManifest(
                ReadinessDeferralManifestError::SelectedUnitNotArtifact { .. }
            ))
        ));
        assert!(matches!(
            progress.readiness_deferral_manifest(&[artifact, artifact]),
            Err(WorkProgressError::ReadinessDeferralManifest(
                ReadinessDeferralManifestError::ArtifactKeysNotSortedUnique
            ))
        ));

        let manifest = progress
            .readiness_deferral_manifest(&[artifact])
            .expect("readiness manifest");
        let row = readiness_row(&progress, manifest.readiness_keys()[0], 701);
        assert!(matches!(
            ReadinessSchedulingObservationRecord::new(
                manifest.clone(),
                progress.execution_epoch(),
                0,
                vec![row.clone()],
            ),
            Err(ReadinessSchedulingObservationError::ZeroObservationGeneration)
        ));
        assert!(matches!(
            ReadinessSchedulingObservationRecord::new(
                manifest.clone(),
                progress.execution_epoch(),
                1,
                Vec::new(),
            ),
            Err(ReadinessSchedulingObservationError::RowCoverageMismatch { .. })
        ));
        assert!(matches!(
            ReadinessSchedulingObservationRecord::new(
                manifest.clone(),
                progress.execution_epoch(),
                1,
                vec![row.clone(), row.clone()],
            ),
            Err(ReadinessSchedulingObservationError::RowCoverageMismatch { .. })
        ));
        assert!(matches!(
            ReadinessTargetSetSummary::new([0; 32], -1),
            Err(ReadinessSchedulingObservationError::NegativeMembershipCount { .. })
        ));
        assert!(matches!(
            DurableReadinessWakeObservation::new(Uuid::nil(), 1),
            Err(ReadinessSchedulingObservationError::NilWakeId)
        ));
        assert!(matches!(
            DurableReadinessWakeObservation::new(Uuid::from_u128(702), 0),
            Err(ReadinessSchedulingObservationError::ZeroWakeOwnerRevision)
        ));
        assert!(matches!(
            ReadinessSchedulingObservationRow::new(
                readiness,
                row.context().clone(),
                row.source_observation().clone(),
                row.source_generation(),
                row.readiness_revision(),
                "   ",
                *row.target_set(),
                row.wake().clone(),
            ),
            Err(ReadinessSchedulingObservationError::EmptyContractVersion)
        ));
        let long_contract_version = "x".repeat(MAX_READINESS_CONTRACT_VERSION_BYTES + 1);
        assert!(matches!(
            ReadinessSchedulingObservationRow::new(
                readiness,
                row.context().clone(),
                row.source_observation().clone(),
                row.source_generation(),
                row.readiness_revision(),
                long_contract_version,
                *row.target_set(),
                row.wake().clone(),
            ),
            Err(ReadinessSchedulingObservationError::ContractVersionTooLong { .. })
        ));
        let padded_contract = ReadinessSchedulingObservationRow::new(
            readiness,
            row.context().clone(),
            row.source_observation().clone(),
            row.source_generation(),
            row.readiness_revision(),
            " readiness-test-contract-v1 ",
            *row.target_set(),
            row.wake().clone(),
        )
        .expect("padded contract version remains exact");
        assert_eq!(
            padded_contract.contract_version(),
            " readiness-test-contract-v1 "
        );

        let mut malformed_digest = serde_json::to_value(
            ReadinessTargetSetSummary::new([0; 32], 0).expect("target summary"),
        )
        .expect("target JSON");
        malformed_digest["membership_digest"] = serde_json::json!(vec![0; 31]);
        assert!(serde_json::from_value::<ReadinessTargetSetSummary>(malformed_digest).is_err());

        let mut negative_generation = serde_json::to_value(&row).expect("row JSON");
        negative_generation["source_generation"] = serde_json::json!(-1);
        let negative_generation: ReadinessSchedulingObservationRow =
            serde_json::from_value(negative_generation).expect("negative generation shape");
        assert!(
            ReadinessSchedulingObservationRecord::new(
                manifest.clone(),
                progress.execution_epoch(),
                1,
                vec![negative_generation],
            )
            .is_err()
        );
        let mut negative_revision = serde_json::to_value(&row).expect("row JSON");
        negative_revision["readiness_revision"] = serde_json::json!(-1);
        let negative_revision: ReadinessSchedulingObservationRow =
            serde_json::from_value(negative_revision).expect("negative revision shape");
        assert!(
            ReadinessSchedulingObservationRecord::new(
                manifest.clone(),
                progress.execution_epoch(),
                1,
                vec![negative_revision],
            )
            .is_err()
        );

        let mut wrong_manifest = serde_json::to_value(&manifest).expect("manifest JSON");
        wrong_manifest["operation_id"] = serde_json::json!(Uuid::from_u128(703).to_string());
        let wrong_manifest: ReadinessDeferralManifest =
            serde_json::from_value(wrong_manifest).expect("wrong manifest shape");
        let wrong_record = ReadinessSchedulingObservationRecord::new(
            wrong_manifest,
            progress.execution_epoch(),
            1,
            vec![row],
        )
        .expect("wrong manifest record shape");
        assert!(matches!(
            progress.validate_readiness_scheduling_observation_record(&wrong_record, &[artifact]),
            Err(WorkProgressError::ReadinessSchedulingObservation(
                ReadinessSchedulingObservationError::Manifest(
                    ReadinessDeferralManifestError::ExpectedManifestMismatch
                )
            ))
        ));
    }

    #[test]
    fn readiness_manifest_rejects_duplicate_wrong_kind_and_invalid_scope_inputs() {
        let progress = copy_progress(true);
        let artifact = progress
            .steps()
            .iter()
            .find(|step| step.requirement() == ParticipantRequirement::OptionalRebuildable)
            .map(WorkProgressStep::key)
            .expect("optional Artifact");

        let mut duplicate_scopes = progress.scopes.clone();
        duplicate_scopes.push(duplicate_scopes[0].clone());
        assert!(matches!(
            super::super::readiness_scheduling::derive_readiness_deferral_manifest(
                progress.operation_id,
                &progress.plan,
                &duplicate_scopes,
                &[artifact],
            ),
            Err(ReadinessDeferralManifestError::DuplicateParticipantScope { .. })
        ));

        let artifact_index = progress
            .scopes
            .iter()
            .position(|scope| scope.participant_id() == artifact.participant_id())
            .expect("Artifact scope");
        let mut wrong_kind_scopes = progress.scopes.clone();
        let artifact_scope = wrong_kind_scopes[artifact_index].clone();
        wrong_kind_scopes[artifact_index] = ParticipantScope {
            participant_id: artifact_scope.participant_id,
            kind: ParticipantKind::Readiness,
            source_fence: artifact_scope.source_fence,
        };
        assert!(matches!(
            super::super::readiness_scheduling::derive_readiness_deferral_manifest(
                progress.operation_id,
                &progress.plan,
                &wrong_kind_scopes,
                &[artifact],
            ),
            Err(ReadinessDeferralManifestError::ScopeKindMismatch {
                expected: ParticipantKind::Artifact,
                actual: ParticipantKind::Readiness,
                ..
            })
        ));

        let mut missing_fence_scopes = progress.scopes.clone();
        let artifact_scope = missing_fence_scopes[artifact_index].clone();
        missing_fence_scopes[artifact_index] = ParticipantScope {
            participant_id: artifact_scope.participant_id,
            kind: artifact_scope.kind,
            source_fence: None,
        };
        assert!(matches!(
            super::super::readiness_scheduling::derive_readiness_deferral_manifest(
                progress.operation_id,
                &progress.plan,
                &missing_fence_scopes,
                &[artifact],
            ),
            Err(ReadinessDeferralManifestError::ArtifactSourceFenceMissing { key })
                if key == artifact
        ));

        let mut invalid_fence_json =
            serde_json::to_value(&progress.scopes[artifact_index]).expect("Artifact scope JSON");
        invalid_fence_json["source_fence"]["lease_epoch"] = serde_json::json!(0);
        let invalid_fence_scope: ParticipantScope =
            serde_json::from_value(invalid_fence_json).expect("invalid fence shape");
        let mut invalid_fence_scopes = progress.scopes.clone();
        invalid_fence_scopes[artifact_index] = invalid_fence_scope;
        assert!(matches!(
            super::super::readiness_scheduling::derive_readiness_deferral_manifest(
                progress.operation_id,
                &progress.plan,
                &invalid_fence_scopes,
                &[artifact],
            ),
            Err(ReadinessDeferralManifestError::ScopeFenceInvalid(
                ParticipantValidationError::InvalidLeaseEpoch
            ))
        ));
    }

    #[test]
    fn readiness_scheduling_requires_current_scopes_and_source_floors_atomically() {
        let mut progress = copy_progress(true);
        let artifact = progress
            .steps()
            .iter()
            .find(|step| step.requirement() == ParticipantRequirement::OptionalRebuildable)
            .map(WorkProgressStep::key)
            .expect("optional Artifact");
        let manifest = progress
            .readiness_deferral_manifest(&[artifact])
            .expect("readiness manifest");
        let readiness = manifest.readiness_keys()[0];
        let valid_row = readiness_row(&progress, readiness, 704);

        let future_context = ReadinessSchedulingObservationRow::new(
            readiness,
            CompletionContext::new(
                progress.execution_epoch() + 1,
                progress.scope_for(readiness).expect("scope").clone(),
            ),
            source_observation(&progress, readiness, 7),
            1,
            1,
            "readiness-test-contract-v1",
            *valid_row.target_set(),
            valid_row.wake().clone(),
        )
        .expect("future context row");
        let future_record = ReadinessSchedulingObservationRecord::new(
            manifest.clone(),
            progress.execution_epoch(),
            1,
            vec![future_context],
        )
        .expect("future context record");
        let before = progress.clone();
        assert!(matches!(
            progress.validate_readiness_scheduling_observation_record(&future_record, &[artifact]),
            Err(WorkProgressError::ReadinessSchedulingContextEpochMismatch { .. })
        ));
        assert_eq!(progress, before);

        let wrong_scope = ReadinessSchedulingObservationRow::new(
            readiness,
            CompletionContext::new(
                progress.execution_epoch(),
                progress
                    .scope_for(artifact)
                    .expect("Artifact scope")
                    .clone(),
            ),
            source_observation(&progress, readiness, 7),
            1,
            1,
            "readiness-test-contract-v1",
            *valid_row.target_set(),
            DurableReadinessWakeObservation::new(Uuid::from_u128(705), 1).expect("wake"),
        )
        .expect("wrong scope row");
        let wrong_scope_record = ReadinessSchedulingObservationRecord::new(
            manifest.clone(),
            progress.execution_epoch(),
            1,
            vec![wrong_scope],
        )
        .expect("wrong scope record");
        assert!(matches!(
            progress
                .validate_readiness_scheduling_observation_record(&wrong_scope_record, &[artifact]),
            Err(WorkProgressError::ReadinessSchedulingScopeMismatch { .. })
        ));

        let wrong_source = SourceRevisionObservation::new(
            SourceId::from_string("other-source"),
            "root-a",
            "db-a",
            1,
            1,
            7,
        )
        .expect("wrong source observation");
        let wrong_source_row = ReadinessSchedulingObservationRow::new(
            readiness,
            CompletionContext::new(
                progress.execution_epoch(),
                progress.scope_for(readiness).expect("scope").clone(),
            ),
            wrong_source,
            1,
            1,
            "readiness-test-contract-v1",
            *valid_row.target_set(),
            DurableReadinessWakeObservation::new(Uuid::from_u128(706), 1).expect("wake"),
        )
        .expect("wrong source row");
        let wrong_source_record = ReadinessSchedulingObservationRecord::new(
            manifest.clone(),
            progress.execution_epoch(),
            1,
            vec![wrong_source_row],
        )
        .expect("wrong source record");
        assert!(matches!(
            progress.validate_readiness_scheduling_observation_record(
                &wrong_source_record,
                &[artifact]
            ),
            Err(WorkProgressError::SourceFenceMismatch {
                role: WorkUnitRole::Readiness
            })
        ));

        let source_read = key_for(&progress, 0, WorkUnitRole::SourceRead);
        progress
            .apply_verified_completion(completion(
                &progress,
                source_read,
                evidence_for(&progress, source_read, 9),
            ))
            .expect("raise source floor");
        let stale_row = readiness_row_at(&progress, readiness, 707, 8);
        let stale_record = ReadinessSchedulingObservationRecord::new(
            manifest,
            progress.execution_epoch(),
            1,
            vec![stale_row],
        )
        .expect("stale source record");
        let before_stale = progress.clone();
        assert!(matches!(
            progress.validate_readiness_scheduling_observation_record(&stale_record, &[artifact]),
            Err(WorkProgressError::SourceRevisionRegressed {
                role: WorkUnitRole::Readiness,
                observed: 8,
                minimum: 9,
            })
        ));
        assert_eq!(progress, before_stale);
    }

    #[test]
    fn normal_success_settlement_enforces_retained_charge_classes_atomically() {
        let retained_classes = [
            FullCapacityAllocationClass::JournalRecord,
            FullCapacityAllocationClass::ExistingDestination,
            FullCapacityAllocationClass::ExistingRecoveryPayload,
        ];
        for (index, allocation_class) in retained_classes.iter().enumerate() {
            let operation_value = 500_u128 + (index as u128 * 10);
            let mut released = copy_progress_with_claim_class(*allocation_class, operation_value);
            complete_required(&mut released);
            let before = released.clone();
            let result = released.apply_verified_claim_settlement(settlement_authority(
                &released,
                settlement_inventory(&released, AdmissionClaimSettlementOutcome::Released),
            ));
            assert_eq!(
                result,
                Err(WorkProgressError::ClaimSettlement(
                    AdmissionClaimValidationError::NormalSuccessRetainedChargeReleased {
                        claim_id: AdmissionClaimId::from_uuid(Uuid::from_u128(201))
                            .expect("claim id"),
                        allocation_class: *allocation_class,
                    }
                ))
            );
            assert_eq!(released, before);

            let mut held = copy_progress_with_claim_class(*allocation_class, operation_value + 1);
            complete_required(&mut held);
            assert_eq!(
                held.apply_verified_claim_settlement(settlement_authority(
                    &held,
                    settlement_inventory(&held, AdmissionClaimSettlementOutcome::Held),
                )),
                Ok(ProgressApplyOutcome::Applied)
            );
            assert_eq!(held.phase(), OperationPhase::ReadinessScheduled);
            assert_eq!(held.disposition(), FullProgressDisposition::None);
            assert_eq!(
                held.claim_settlement()
                    .expect("held settlement")
                    .settlements()[0]
                    .outcome(),
                AdmissionClaimSettlementOutcome::Held
            );

            let mut consumed =
                copy_progress_with_claim_class(*allocation_class, operation_value + 2);
            complete_required(&mut consumed);
            assert_eq!(
                consumed.apply_verified_claim_settlement(settlement_authority(
                    &consumed,
                    settlement_inventory(
                        &consumed,
                        AdmissionClaimSettlementOutcome::ConsumedOrTransferred,
                    ),
                )),
                Ok(ProgressApplyOutcome::Applied)
            );
            assert_eq!(consumed.phase(), OperationPhase::Terminal);
            assert_eq!(consumed.disposition(), FullProgressDisposition::Succeeded);
        }

        let unrelated_classes = [
            FullCapacityAllocationClass::DestinationStaging,
            FullCapacityAllocationClass::OperationRecoveryPayload,
            FullCapacityAllocationClass::SourceDatabaseCommit,
            FullCapacityAllocationClass::SourceWalShm,
            FullCapacityAllocationClass::GlobalDatabaseCommit,
            FullCapacityAllocationClass::GlobalWalShm,
        ];
        for (index, allocation_class) in unrelated_classes.iter().enumerate() {
            let mut progress =
                copy_progress_with_claim_class(*allocation_class, 600_u128 + index as u128);
            complete_required(&mut progress);
            progress
                .apply_verified_claim_settlement(settlement_authority(
                    &progress,
                    settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Released),
                ))
                .expect("unrelated released class remains allowed");
            assert_eq!(progress.disposition(), FullProgressDisposition::Succeeded);
        }
    }

    #[test]
    fn claim_settlement_requires_admission_and_resolved_work() {
        let mut unadmitted = copy_progress(false);
        complete_required(&mut unadmitted);
        assert_eq!(unadmitted.phase(), OperationPhase::ReadinessScheduled);
        let candidate = AdmissionClaimSettlementInventory::new(vec![
            AdmissionClaimSettlement::new(
                AdmissionClaimId::from_uuid(Uuid::from_u128(201)).expect("claim id"),
                AdmissionClaimSettlementOutcome::Released,
            )
            .expect("settlement"),
        ])
        .expect("candidate settlement");
        let authority = VerifiedClaimSettlement {
            operation_id: unadmitted.operation_id,
            plan: unadmitted.plan.clone(),
            execution_epoch: unadmitted.execution_epoch,
            admission_claims: admission_claims(12),
            settlement: candidate,
        };
        let before = unadmitted.clone();
        assert_eq!(
            unadmitted.apply_verified_claim_settlement(authority),
            Err(WorkProgressError::ClaimSettlementAdmissionClaimsMissing)
        );
        assert_eq!(unadmitted, before);

        let mut incomplete = copy_progress_with_admission_claims();
        let before = incomplete.clone();
        let settlement =
            settlement_inventory(&incomplete, AdmissionClaimSettlementOutcome::Released);
        assert_eq!(
            incomplete
                .apply_verified_claim_settlement(settlement_authority(&incomplete, settlement)),
            Err(WorkProgressError::ClaimSettlementProgressIncomplete)
        );
        assert_eq!(incomplete, before);
    }

    #[test]
    fn claim_settlement_requires_exact_nonheld_coverage_before_mutation() {
        let mut progress = copy_progress_with_two_admission_claims();
        complete_required(&mut progress);

        let partial = AdmissionClaimSettlementInventory::new(vec![
            AdmissionClaimSettlement::new(
                AdmissionClaimId::from_uuid(Uuid::from_u128(201)).expect("claim id"),
                AdmissionClaimSettlementOutcome::Released,
            )
            .expect("partial settlement"),
        ])
        .expect("partial settlement shape");
        let before = progress.clone();
        assert!(matches!(
            progress.apply_verified_claim_settlement(settlement_authority(&progress, partial)),
            Err(WorkProgressError::ClaimSettlement(
                AdmissionClaimValidationError::SettlementCountMismatch { .. }
            ))
        ));
        assert_eq!(progress, before);

        let unknown = AdmissionClaimSettlementInventory::new(vec![
            AdmissionClaimSettlement::new(
                AdmissionClaimId::from_uuid(Uuid::from_u128(201)).expect("claim id"),
                AdmissionClaimSettlementOutcome::Released,
            )
            .expect("known settlement"),
            AdmissionClaimSettlement::new(
                AdmissionClaimId::from_uuid(Uuid::from_u128(299)).expect("unknown claim id"),
                AdmissionClaimSettlementOutcome::Released,
            )
            .expect("unknown settlement"),
        ])
        .expect("unknown settlement shape");
        let before = progress.clone();
        assert!(matches!(
            progress.apply_verified_claim_settlement(settlement_authority(&progress, unknown)),
            Err(WorkProgressError::ClaimSettlement(
                AdmissionClaimValidationError::UnknownSettlementClaim { .. }
            ))
        ));
        assert_eq!(progress, before);

        let held = settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Held);
        let before = progress.clone();
        assert_eq!(
            progress.apply_verified_claim_settlement(settlement_authority(&progress, held)),
            Ok(ProgressApplyOutcome::Applied)
        );
        assert_ne!(progress, before);
        assert_eq!(progress.phase(), OperationPhase::ReadinessScheduled);
        assert_eq!(progress.disposition(), FullProgressDisposition::None);
        let retained = progress
            .claim_settlement()
            .expect("held settlement retained");
        assert!(!retained.all_settled_without_held());
        let released = settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Released);
        progress
            .apply_verified_claim_settlement(settlement_authority(&progress, released))
            .expect("held claim settles later");
        assert_eq!(progress.disposition(), FullProgressDisposition::Succeeded);
    }

    #[test]
    fn claim_settlement_allows_only_held_rows_to_advance() {
        let mut progress = copy_progress_with_two_admission_claims();
        complete_required(&mut progress);
        let first = AdmissionClaimSettlementInventory::new(vec![
            AdmissionClaimSettlement::new(
                AdmissionClaimId::from_uuid(Uuid::from_u128(201)).expect("first claim id"),
                AdmissionClaimSettlementOutcome::Released,
            )
            .expect("released first claim"),
            AdmissionClaimSettlement::new(
                AdmissionClaimId::from_uuid(Uuid::from_u128(202)).expect("second claim id"),
                AdmissionClaimSettlementOutcome::Held,
            )
            .expect("held second claim"),
        ])
        .expect("partial settlement");
        progress
            .apply_verified_claim_settlement(settlement_authority(&progress, first))
            .expect("retain partial settlement");
        assert_eq!(progress.phase(), OperationPhase::ReadinessScheduled);
        assert_eq!(progress.disposition(), FullProgressDisposition::None);

        let rewrite_settled = AdmissionClaimSettlementInventory::new(vec![
            AdmissionClaimSettlement::new(
                AdmissionClaimId::from_uuid(Uuid::from_u128(201)).expect("first claim id"),
                AdmissionClaimSettlementOutcome::ConsumedOrTransferred,
            )
            .expect("rewritten first claim"),
            AdmissionClaimSettlement::new(
                AdmissionClaimId::from_uuid(Uuid::from_u128(202)).expect("second claim id"),
                AdmissionClaimSettlementOutcome::Held,
            )
            .expect("held second claim"),
        ])
        .expect("rewrite settlement");
        let before_rewrite = progress.clone();
        assert_eq!(
            progress
                .apply_verified_claim_settlement(settlement_authority(&progress, rewrite_settled,)),
            Err(WorkProgressError::ClaimSettlementReplayConflict)
        );
        assert_eq!(progress, before_rewrite);

        let completed = AdmissionClaimSettlementInventory::new(vec![
            AdmissionClaimSettlement::new(
                AdmissionClaimId::from_uuid(Uuid::from_u128(201)).expect("first claim id"),
                AdmissionClaimSettlementOutcome::Released,
            )
            .expect("released first claim"),
            AdmissionClaimSettlement::new(
                AdmissionClaimId::from_uuid(Uuid::from_u128(202)).expect("second claim id"),
                AdmissionClaimSettlementOutcome::ConsumedOrTransferred,
            )
            .expect("consumed second claim"),
        ])
        .expect("completed settlement");
        progress
            .apply_verified_claim_settlement(settlement_authority(&progress, completed))
            .expect("settle previously held claim");
        assert_eq!(progress.disposition(), FullProgressDisposition::Succeeded);
    }

    #[test]
    fn claim_settlement_rejects_stale_authority_and_suspended_dispositions() {
        let mut progress = copy_progress_with_admission_claims();
        complete_required(&mut progress);
        let settlement = settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Released);

        let mut stale_operation = settlement_authority(&progress, settlement.clone());
        stale_operation.operation_id = OperationId::from_uuid(Uuid::from_u128(999));
        let before = progress.clone();
        assert_eq!(
            progress.apply_verified_claim_settlement(stale_operation),
            Err(WorkProgressError::ClaimSettlementAuthorityMismatch)
        );
        assert_eq!(progress, before);

        let mut stale_plan = settlement_authority(&progress, settlement.clone());
        stale_plan.plan = copy_progress_with_admission_claims_and_optional()
            .plan
            .clone();
        assert_eq!(
            progress.apply_verified_claim_settlement(stale_plan),
            Err(WorkProgressError::ClaimSettlementAuthorityMismatch)
        );
        assert_eq!(progress, before);

        let mut stale_epoch = settlement_authority(&progress, settlement.clone());
        stale_epoch.execution_epoch = 1;
        assert_eq!(
            progress.apply_verified_claim_settlement(stale_epoch),
            Err(WorkProgressError::ClaimSettlementEpochMismatch {
                expected: 0,
                actual: 1,
            })
        );
        assert_eq!(progress, before);

        let mut stale_claims = settlement_authority(&progress, settlement.clone());
        stale_claims.admission_claims = admission_claims(11);
        assert_eq!(
            progress.apply_verified_claim_settlement(stale_claims),
            Err(WorkProgressError::ClaimSettlementClaimsMismatch)
        );
        assert_eq!(progress, before);

        let retry_observation = RetryObservation::new(
            1,
            0,
            1,
            RetryLeaseId::new("settlement-retry").expect("retry lease"),
            10,
            RetryErrorClass::TransientContention,
            "busy",
            None,
        )
        .expect("retry observation");
        progress
            .request_retry(retry_observation)
            .expect("request retry");
        let before_retry = progress.clone();
        assert!(matches!(
            progress.apply_verified_claim_settlement(settlement_authority(&progress, settlement)),
            Err(WorkProgressError::ProgressSuspended {
                disposition: FullProgressDisposition::RetryPending
            })
        ));
        assert_eq!(progress, before_retry);

        let mut audit = copy_progress_with_admission_claims();
        complete_required(&mut audit);
        audit
            .require_audit(ProgressAuditObservation::new(
                ProgressAuditReason::RevisionGap,
            ))
            .expect("require audit");
        let settlement = settlement_inventory(&audit, AdmissionClaimSettlementOutcome::Released);
        let before_audit = audit.clone();
        assert!(matches!(
            audit.apply_verified_claim_settlement(settlement_authority(&audit, settlement)),
            Err(WorkProgressError::ProgressSuspended {
                disposition: FullProgressDisposition::AuditRequired
            })
        ));
        assert_eq!(audit, before_audit);

        let mut cancelled = copy_progress_with_admission_claims();
        complete_required(&mut cancelled);
        cancelled
            .request_cancellation(CancellationRequestObservation::new(301).expect("cancel"))
            .expect("request cancellation");
        let settlement =
            settlement_inventory(&cancelled, AdmissionClaimSettlementOutcome::Released);
        let before_cancel = cancelled.clone();
        assert!(matches!(
            cancelled.apply_verified_claim_settlement(settlement_authority(&cancelled, settlement)),
            Err(WorkProgressError::ProgressSuspended {
                disposition: FullProgressDisposition::CancelRequestedAfterPublish
            })
        ));
        assert_eq!(cancelled, before_cancel);
    }

    #[test]
    fn claim_settlement_replay_is_stable_and_settled_rows_are_immutable() {
        let mut progress = copy_progress_with_admission_claims();
        complete_required(&mut progress);
        let settlement = settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Released);
        progress
            .apply_verified_claim_settlement(settlement_authority(&progress, settlement.clone()))
            .expect("first settlement");
        assert_eq!(progress.disposition(), FullProgressDisposition::Succeeded);
        assert_eq!(progress.claim_settlement(), Some(&settlement));

        let after_success = progress.clone();
        assert_eq!(
            progress.apply_verified_claim_settlement(settlement_authority(&progress, settlement)),
            Ok(ProgressApplyOutcome::Replay)
        );
        assert_eq!(progress, after_success);

        let swapped = settlement_inventory(
            &progress,
            AdmissionClaimSettlementOutcome::ConsumedOrTransferred,
        );
        assert_eq!(
            progress.apply_verified_claim_settlement(settlement_authority(&progress, swapped)),
            Err(WorkProgressError::ClaimSettlementReplayConflict)
        );
        assert_eq!(progress, after_success);

        let held = settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Held);
        assert_eq!(
            progress.apply_verified_claim_settlement(settlement_authority(&progress, held)),
            Err(WorkProgressError::ClaimSettlementReplayConflict)
        );
        assert_eq!(progress, after_success);
    }

    #[test]
    fn empty_retry_window_is_rejected_without_changing_progress() {
        let mut progress = copy_progress(false);
        let before = progress.clone();
        let observation = RetryObservation::new(
            1,
            0,
            1,
            RetryLeaseId::new("empty-window").expect("lease"),
            1,
            RetryErrorClass::TransientContention,
            "busy",
            None,
        )
        .expect("legacy observation remains decodable");
        assert_eq!(
            progress.request_retry(observation),
            Err(WorkProgressError::EmptyRetryWindow)
        );
        assert_eq!(progress, before);
    }

    #[test]
    fn retry_audit_and_cancellation_preserve_guarded_progress() {
        let mut retry = copy_progress(false);
        let observation = RetryObservation::new(
            1,
            0,
            1,
            RetryLeaseId::new("retry-1").expect("retry lease"),
            2,
            RetryErrorClass::TransientContention,
            "busy",
            None,
        )
        .expect("retry observation");
        retry
            .request_retry(observation.clone())
            .expect("request retry");
        let key = key_for(&retry, 0, WorkUnitRole::SourceRead);
        let evidence = evidence_for(&retry, key, 7);
        let mut delayed_completion = completion(&retry, key, evidence.clone());
        assert!(matches!(
            retry.apply_verified_completion(delayed_completion),
            Err(WorkProgressError::ProgressSuspended {
                disposition: FullProgressDisposition::RetryPending
            })
        ));
        delayed_completion = completion(&retry, key, evidence.clone());
        retry
            .resume_retry(VerifiedRetryResume {
                operation_id: retry.operation_id,
                plan: retry.plan.clone(),
                retry_observation: observation.clone(),
                now_unix_ms: 1,
                expected_execution_epoch: 0,
            })
            .expect("retry lease reacquisition");
        assert_eq!(retry.execution_epoch(), 1);
        assert!(retry.retry_observation().is_none());
        assert_eq!(
            retry
                .last_retry_observation()
                .expect("last retry observation")
                .attempt(),
            1
        );
        delayed_completion.execution_epoch = 0;
        assert!(matches!(
            retry.apply_verified_completion(delayed_completion),
            Err(WorkProgressError::ExecutionEpochMismatch {
                expected: 1,
                actual: 0
            })
        ));
        assert_eq!(
            retry
                .apply_verified_completion(completion(&retry, key, evidence.clone()))
                .expect("retry completion"),
            ProgressApplyOutcome::Applied
        );
        assert_eq!(retry.disposition(), FullProgressDisposition::None);
        assert!(retry.retry_observation().is_none());
        assert_eq!(
            retry
                .apply_verified_completion(completion(&retry, key, evidence))
                .expect("idempotent replay"),
            ProgressApplyOutcome::Replay
        );
        assert!(matches!(
            retry.apply_verified_completion(completion(
                &retry,
                key,
                StoredStepEvidence::SourceObserved {
                    observation: source_observation(&retry, key, 8),
                },
            )),
            Err(WorkProgressError::CompletionReplayConflict { key: replay_key })
                if replay_key == key
        ));
        retry
            .request_retry(observation.clone())
            .expect("exact retry replay");
        assert_eq!(retry.disposition(), FullProgressDisposition::None);
        let same_attempt_different_retry = RetryObservation::new(
            1,
            0,
            1,
            RetryLeaseId::new("retry-1-different").expect("retry lease"),
            2,
            RetryErrorClass::TransientContention,
            "busy",
            None,
        )
        .expect("retry observation");
        assert!(matches!(
            retry.request_retry(same_attempt_different_retry),
            Err(WorkProgressError::RetryReplayConflict { attempt: 1 })
        ));
        let second_retry = RetryObservation::new(
            2,
            2,
            3,
            RetryLeaseId::new("retry-2").expect("retry lease"),
            4,
            RetryErrorClass::TransientAvailability,
            "again",
            None,
        )
        .expect("second retry observation");
        retry
            .request_retry(second_retry.clone())
            .expect("newer retry attempt");
        assert_eq!(
            retry
                .last_retry_observation()
                .expect("newest retry observation")
                .attempt(),
            2
        );

        let mut delayed_retry = copy_progress(false);
        let delayed_observation = RetryObservation::new(
            1,
            0,
            5,
            RetryLeaseId::new("retry-delayed").expect("retry lease"),
            10,
            RetryErrorClass::TransientContention,
            "delayed",
            None,
        )
        .expect("delayed retry observation");
        delayed_retry
            .request_retry(delayed_observation.clone())
            .expect("delayed retry request");
        assert!(matches!(
            delayed_retry.resume_retry(VerifiedRetryResume {
                operation_id: delayed_retry.operation_id,
                plan: delayed_retry.plan.clone(),
                retry_observation: delayed_observation.clone(),
                now_unix_ms: 4,
                expected_execution_epoch: 0,
            }),
            Err(WorkProgressError::RetryResumeWindowInvalid {
                now: 4,
                next_retry: 5,
                lease_expiry: 10,
            })
        ));
        delayed_retry
            .resume_retry(VerifiedRetryResume {
                operation_id: delayed_retry.operation_id,
                plan: delayed_retry.plan.clone(),
                retry_observation: delayed_observation,
                now_unix_ms: 5,
                expected_execution_epoch: 0,
            })
            .expect("on-time retry lease reacquisition");
        assert_eq!(delayed_retry.execution_epoch(), 1);

        let mut audit = copy_progress(false);
        publish_prefix(&mut audit);
        audit
            .require_audit(ProgressAuditObservation::new(
                ProgressAuditReason::UncertainEvidence,
            ))
            .expect("require audit");
        let audit_key = key_for(&audit, 0, WorkUnitRole::SourceAffect);
        assert!(matches!(
            audit.apply_verified_completion(completion(
                &audit,
                audit_key,
                evidence_for(&audit, audit_key, 7),
            )),
            Err(WorkProgressError::ProgressSuspended {
                disposition: FullProgressDisposition::AuditRequired
            })
        ));

        let mut cancelled = copy_progress_with_admission_claims();
        let request = CancellationRequestObservation::new(42).expect("cancel request");
        cancelled
            .request_cancellation(request)
            .expect("request cancellation");
        cancelled
            .request_cancellation(request)
            .expect("idempotent cancellation replay");
        assert!(matches!(
            cancelled.request_cancellation(CancellationRequestObservation::new(43).unwrap()),
            Err(WorkProgressError::CancellationReplayConflict)
        ));
        let cleanup = PrePublishCleanupObservation::new(Vec::new()).expect("empty cleanup");
        let settlement =
            settlement_inventory(&cancelled, AdmissionClaimSettlementOutcome::Released);
        let authority = pre_publish_cleanup_authority(&cancelled, cleanup, settlement);
        let replay = authority.clone();
        cancelled
            .finish_pre_publish_cancellation(authority)
            .expect("empty prepublication cancellation can finish");
        assert_eq!(cancelled.phase(), OperationPhase::Terminal);
        assert_eq!(
            cancelled.disposition(),
            FullProgressDisposition::CancelledBeforePublish
        );
        let after_outcome = cancelled.clone();
        cancelled
            .finish_pre_publish_cancellation(replay)
            .expect("original authority replay");
        assert_eq!(cancelled, after_outcome);
        assert_eq!(
            cancelled.disposition(),
            FullProgressDisposition::CancelledBeforePublish
        );
    }

    #[test]
    fn prepublish_cleanup_retains_uncertainty_and_requires_staging_absence_for_release() {
        let mut progress = copy_progress_with_admission_claims();
        apply_role(&mut progress, 0, WorkUnitRole::SourceRead);
        apply_role(&mut progress, 0, WorkUnitRole::Prepare);
        progress
            .request_cancellation(CancellationRequestObservation::new(401).expect("cancel"))
            .expect("prepublication cancellation");

        let partial_cleanup =
            PrePublishCleanupObservation::new(vec![PrePublishActionCleanup::new(
                0,
                Some(PrePublishFinalClaimObservation::NoFinalClaim),
                Some(PrePublishOriginalStateObservation::Preserved),
                Some(PrePublishAbsenceObservation::Preserved),
                None,
            )])
            .expect("partial cleanup");
        let partial_authority = pre_publish_cleanup_authority(
            &progress,
            partial_cleanup,
            settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Held),
        );
        let original_authority = partial_authority.clone();
        progress
            .finish_pre_publish_cancellation(partial_authority)
            .expect("retain partial cleanup");
        assert_eq!(progress.recovery_generation(), 1);
        assert_eq!(
            progress.disposition(),
            FullProgressDisposition::CancelRequestedBeforePublish
        );

        let after_partial = progress.clone();
        progress
            .finish_pre_publish_cancellation(original_authority)
            .expect("replay original authority after acknowledgement loss");
        assert_eq!(progress, after_partial);

        let release_before_absence = pre_publish_cleanup_authority(
            &progress,
            progress
                .pre_publish_cancellation()
                .expect("partial observation")
                .cleanup()
                .clone(),
            settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Released),
        );
        let before_rejected_release = progress.clone();
        assert_eq!(
            progress.finish_pre_publish_cancellation(release_before_absence),
            Err(
                WorkProgressError::PrePublishCancellationSettlementCleanupIncomplete {
                    allocation_class: FullCapacityAllocationClass::DestinationStaging,
                }
            )
        );
        assert_eq!(progress, before_rejected_release);

        let complete_cleanup =
            PrePublishCleanupObservation::new(vec![PrePublishActionCleanup::new(
                0,
                Some(PrePublishFinalClaimObservation::NoFinalClaim),
                Some(PrePublishOriginalStateObservation::Preserved),
                Some(PrePublishAbsenceObservation::Absent),
                None,
            )])
            .expect("complete cleanup");
        progress
            .finish_pre_publish_cancellation(pre_publish_cleanup_authority(
                &progress,
                complete_cleanup,
                settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Released),
            ))
            .expect("finish prepublication cancellation");
        assert_eq!(progress.phase(), OperationPhase::Terminal);
        assert_eq!(
            progress.disposition(),
            FullProgressDisposition::CancelledBeforePublish
        );
        let terminal = progress.clone();
        let terminal_authority = pre_publish_cleanup_authority(
            &progress,
            progress
                .pre_publish_cancellation()
                .expect("terminal observation")
                .cleanup()
                .clone(),
            progress
                .pre_publish_cancellation()
                .expect("terminal observation")
                .settlement()
                .clone(),
        );
        progress
            .finish_pre_publish_cancellation(terminal_authority)
            .expect("terminal replay");
        assert_eq!(progress, terminal);
    }

    #[test]
    fn prepublish_cleanup_holds_uncheckpointed_first_effect_boundary() {
        let mut progress = copy_progress_with_two_admission_claims();
        apply_role(&mut progress, 0, WorkUnitRole::SourceRead);
        progress
            .request_cancellation(CancellationRequestObservation::new(408).expect("cancel"))
            .expect("prepublication cancellation");

        let cleanup = PrePublishCleanupObservation::new(Vec::new()).expect("empty cleanup");
        let held = settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Held);
        progress
            .finish_pre_publish_cancellation(pre_publish_cleanup_authority(
                &progress,
                cleanup.clone(),
                held,
            ))
            .expect("retain uncheckpointed boundary");
        assert_eq!(
            progress.disposition(),
            FullProgressDisposition::CancelRequestedBeforePublish
        );
        assert_eq!(progress.recovery_generation(), 1);

        let before_staging_release = progress.clone();
        assert_eq!(
            progress.finish_pre_publish_cancellation(pre_publish_cleanup_authority(
                &progress,
                cleanup.clone(),
                settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Released),
            )),
            Err(WorkProgressError::PrePublishCancellationSettlementGateIncomplete)
        );
        assert_eq!(progress, before_staging_release);

        let before_recovery_release = progress.clone();
        assert_eq!(
            progress.finish_pre_publish_cancellation(pre_publish_cleanup_authority(
                &progress,
                cleanup,
                settlement_inventory_with_outcomes(
                    &progress,
                    &[
                        AdmissionClaimSettlementOutcome::Held,
                        AdmissionClaimSettlementOutcome::Released,
                    ],
                ),
            )),
            Err(WorkProgressError::PrePublishCancellationSettlementGateIncomplete)
        );
        assert_eq!(progress, before_recovery_release);
    }

    #[test]
    fn prepublish_unstarted_quiescence_requires_decisive_barrier_and_is_replay_stable() {
        let mut progress = copy_progress_with_two_admission_claims();
        apply_role(&mut progress, 0, WorkUnitRole::SourceRead);
        progress
            .request_cancellation(CancellationRequestObservation::new(409).expect("cancel"))
            .expect("prepublication cancellation");

        let uncertain = unstarted_quiescence_for_action(
            &progress,
            0,
            PrePublishUnstartedQuiescenceResult::Uncertain,
            Some(PrePublishAbsenceObservation::Preserved),
            Some(PrePublishAbsenceObservation::Preserved),
        );
        progress
            .finish_pre_publish_cancellation(pre_publish_cleanup_authority_with_quiescence(
                &progress,
                uncertain,
                PrePublishCleanupObservation::new(Vec::new()).expect("empty cleanup"),
                settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Held),
            ))
            .expect("retain uncertain boundary");
        assert_eq!(progress.recovery_generation(), 1);
        assert_eq!(
            progress.disposition(),
            FullProgressDisposition::CancelRequestedBeforePublish
        );

        let decisive = unstarted_quiescence_for_action(
            &progress,
            0,
            PrePublishUnstartedQuiescenceResult::QuiescedWithoutEntry,
            Some(PrePublishAbsenceObservation::Absent),
            Some(PrePublishAbsenceObservation::Absent),
        );
        let decisive_authority = pre_publish_cleanup_authority_with_quiescence(
            &progress,
            decisive,
            PrePublishCleanupObservation::new(Vec::new()).expect("empty cleanup"),
            settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Held),
        );
        let decisive_replay = decisive_authority.clone();
        progress
            .finish_pre_publish_cancellation(decisive_authority)
            .expect("retain decisive boundary");
        assert_eq!(progress.recovery_generation(), 2);
        let after_decisive = progress.clone();
        progress
            .finish_pre_publish_cancellation(decisive_replay)
            .expect("acknowledgement-loss replay");
        assert_eq!(progress, after_decisive);

        let regressed = unstarted_quiescence_for_action(
            &progress,
            0,
            PrePublishUnstartedQuiescenceResult::Uncertain,
            Some(PrePublishAbsenceObservation::Preserved),
            Some(PrePublishAbsenceObservation::Preserved),
        );
        let before_regression = progress.clone();
        assert_eq!(
            progress.finish_pre_publish_cancellation(
                pre_publish_cleanup_authority_with_quiescence(
                    &progress,
                    regressed,
                    PrePublishCleanupObservation::new(Vec::new()).expect("empty cleanup"),
                    settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Held),
                )
            ),
            Err(WorkProgressError::PrePublishCancellationQuiescenceReplayConflict)
        );
        assert_eq!(progress, before_regression);

        progress
            .finish_pre_publish_cancellation(pre_publish_cleanup_authority_with_quiescence(
                &progress,
                unstarted_quiescence_for_action(
                    &progress,
                    0,
                    PrePublishUnstartedQuiescenceResult::QuiescedWithoutEntry,
                    Some(PrePublishAbsenceObservation::Absent),
                    Some(PrePublishAbsenceObservation::Absent),
                ),
                PrePublishCleanupObservation::new(Vec::new()).expect("empty cleanup"),
                settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Released),
            ))
            .expect("decisive absence and settled claims finish cancellation");
        assert_eq!(
            progress.disposition(),
            FullProgressDisposition::CancelledBeforePublish
        );
    }

    #[test]
    fn prepublish_unstarted_quiescence_rejects_wrong_rows_and_bounds() {
        let mut progress = copy_progress_with_admission_claims();
        let source_key = key_for(&progress, 0, WorkUnitRole::SourceRead);
        let wrong_action_key = WorkUnitKey::new(
            1,
            source_key.step_ordinal(),
            source_key.participant_id(),
            source_key.role(),
        );

        let duplicate_rows = PrePublishUnstartedQuiescenceAction::new(
            0,
            Vec::new(),
            PrePublishUnstartedQuiescenceResult::Uncertain,
            None,
            None,
        );
        assert!(matches!(
            PrePublishUnstartedQuiescenceObservation::new(vec![
                duplicate_rows.clone(),
                duplicate_rows.clone(),
            ]),
            Err(PrePublishUnstartedQuiescenceValidationError::ActionsNotSortedOrUnique { .. })
        ));
        assert!(matches!(
            PrePublishUnstartedQuiescenceObservation::new(vec![
                duplicate_rows;
                MAX_PRE_PUBLISH_UNSTARTED_QUIESCENCE_ACTIONS
                    + 1
            ]),
            Err(PrePublishUnstartedQuiescenceValidationError::TooManyActions { .. })
        ));
        assert!(matches!(
            PrePublishUnstartedQuiescenceObservation::new(vec![
                PrePublishUnstartedQuiescenceAction::new(
                    0,
                    vec![source_key; MAX_PRE_PUBLISH_UNSTARTED_QUIESCENCE_KEYS + 1],
                    PrePublishUnstartedQuiescenceResult::Uncertain,
                    None,
                    None,
                ),
            ]),
            Err(PrePublishUnstartedQuiescenceValidationError::TooManyKeys { .. })
        ));
        assert!(matches!(
            PrePublishUnstartedQuiescenceObservation::new(vec![
                PrePublishUnstartedQuiescenceAction::new(
                    0,
                    vec![source_key, source_key],
                    PrePublishUnstartedQuiescenceResult::Uncertain,
                    None,
                    None,
                ),
            ]),
            Err(PrePublishUnstartedQuiescenceValidationError::KeysNotSortedOrUnique { .. })
        ));

        apply_role(&mut progress, 0, WorkUnitRole::SourceRead);
        progress
            .request_cancellation(CancellationRequestObservation::new(410).expect("cancel"))
            .expect("prepublication cancellation");
        let unknown_action = PrePublishUnstartedQuiescenceObservation::new(vec![
            PrePublishUnstartedQuiescenceAction::new(
                99,
                vec![wrong_action_key],
                PrePublishUnstartedQuiescenceResult::Uncertain,
                None,
                None,
            ),
        ])
        .expect("bounded unknown action");
        let held = settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Held);
        let before = progress.clone();
        assert!(matches!(
            progress.finish_pre_publish_cancellation(
                pre_publish_cleanup_authority_with_quiescence(
                    &progress,
                    unknown_action,
                    PrePublishCleanupObservation::new(Vec::new()).expect("empty cleanup"),
                    held.clone(),
                )
            ),
            Err(
                WorkProgressError::PrePublishUnstartedQuiescenceActionSetMismatch {
                    action_ordinal: 99
                }
            )
        ));
        assert_eq!(progress, before);

        let wrong_keys = PrePublishUnstartedQuiescenceObservation::new(vec![
            PrePublishUnstartedQuiescenceAction::new(
                0,
                vec![wrong_action_key],
                PrePublishUnstartedQuiescenceResult::Uncertain,
                None,
                None,
            ),
        ])
        .expect("bounded cross-action key");
        assert!(matches!(
            progress.finish_pre_publish_cancellation(
                pre_publish_cleanup_authority_with_quiescence(
                    &progress,
                    wrong_keys,
                    PrePublishCleanupObservation::new(Vec::new()).expect("empty cleanup"),
                    held.clone(),
                )
            ),
            Err(
                WorkProgressError::PrePublishUnstartedQuiescenceKeySetMismatch {
                    action_ordinal: 0
                }
            )
        ));
        assert_eq!(progress, before);

        let unexpected_recovery = unstarted_quiescence_for_action(
            &progress,
            0,
            PrePublishUnstartedQuiescenceResult::Uncertain,
            None,
            Some(PrePublishAbsenceObservation::Absent),
        );
        assert!(matches!(
            progress.finish_pre_publish_cancellation(
                pre_publish_cleanup_authority_with_quiescence(
                    &progress,
                    unexpected_recovery,
                    PrePublishCleanupObservation::new(Vec::new()).expect("empty cleanup"),
                    held.clone(),
                )
            ),
            Err(
                WorkProgressError::PrePublishUnstartedQuiescenceRecoveryUnexpected {
                    action_ordinal: 0
                }
            )
        ));
        assert_eq!(progress, before);

        let mut cleanable = copy_progress_with_admission_claims();
        apply_role(&mut cleanable, 0, WorkUnitRole::SourceRead);
        apply_role(&mut cleanable, 0, WorkUnitRole::Prepare);
        cleanable
            .request_cancellation(CancellationRequestObservation::new(412).expect("cancel"))
            .expect("prepublication cancellation");
        let non_not_started = PrePublishUnstartedQuiescenceObservation::new(vec![
            PrePublishUnstartedQuiescenceAction::new(
                0,
                Vec::new(),
                PrePublishUnstartedQuiescenceResult::Uncertain,
                None,
                None,
            ),
        ])
        .expect("bounded cleanable action row");
        assert!(matches!(
            cleanable.finish_pre_publish_cancellation(
                pre_publish_cleanup_authority_with_quiescence(
                    &cleanable,
                    non_not_started,
                    PrePublishCleanupObservation::new(Vec::new()).expect("empty cleanup"),
                    settlement_inventory(&cleanable, AdmissionClaimSettlementOutcome::Held),
                )
            ),
            Err(
                WorkProgressError::PrePublishUnstartedQuiescenceActionSetMismatch {
                    action_ordinal: 0
                }
            )
        ));

        let not_started_cleanup =
            PrePublishCleanupObservation::new(vec![PrePublishActionCleanup::new(
                0,
                Some(PrePublishFinalClaimObservation::NoFinalClaim),
                Some(PrePublishOriginalStateObservation::Preserved),
                None,
                None,
            )])
            .expect("bounded not-started cleanup row");
        assert!(matches!(
            progress.finish_pre_publish_cancellation(pre_publish_cleanup_authority(
                &progress,
                not_started_cleanup,
                held,
            )),
            Err(WorkProgressError::PrePublishCleanupActionSetMismatch { action_ordinal: 0 })
        ));
        assert_eq!(progress, before);
    }

    #[test]
    fn prepublish_unstarted_quiescence_requires_current_epoch_after_reacquisition() {
        let mut progress = copy_progress_with_two_admission_claims();
        apply_role(&mut progress, 0, WorkUnitRole::SourceRead);
        progress
            .request_cancellation(CancellationRequestObservation::new(411).expect("cancel"))
            .expect("prepublication cancellation");
        let retained_quiescence = unstarted_quiescence_for_action(
            &progress,
            0,
            PrePublishUnstartedQuiescenceResult::QuiescedWithoutEntry,
            Some(PrePublishAbsenceObservation::Absent),
            Some(PrePublishAbsenceObservation::Absent),
        );
        progress
            .finish_pre_publish_cancellation(pre_publish_cleanup_authority_with_quiescence(
                &progress,
                retained_quiescence.clone(),
                PrePublishCleanupObservation::new(Vec::new()).expect("empty cleanup"),
                settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Held),
            ))
            .expect("retain current epoch proof");
        let stale_authority = pre_publish_cleanup_authority_with_quiescence(
            &progress,
            retained_quiescence,
            PrePublishCleanupObservation::new(Vec::new()).expect("empty cleanup"),
            settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Held),
        );
        let target = key_for(&progress, 0, WorkUnitRole::SourceRead).participant_id();
        progress
            .reacquire_source(source_reacquisition_authority(&progress, target, 7, 2, 0))
            .expect("source reacquisition");
        let after_reacquisition = progress.clone();
        assert_eq!(
            progress.finish_pre_publish_cancellation(stale_authority),
            Err(WorkProgressError::PrePublishCancellationEpochMismatch {
                expected: 1,
                actual: 0,
            })
        );
        assert_eq!(progress, after_reacquisition);
        assert_eq!(progress.recovery_generation(), 1);
        assert_eq!(
            progress
                .pre_publish_cancellation()
                .expect("retained quiescence")
                .unstarted_quiescence()
                .actions()
                .len(),
            1
        );

        let current_quiescence = unstarted_quiescence_for_action(
            &progress,
            0,
            PrePublishUnstartedQuiescenceResult::QuiescedWithoutEntry,
            Some(PrePublishAbsenceObservation::Absent),
            Some(PrePublishAbsenceObservation::Absent),
        );
        progress
            .finish_pre_publish_cancellation(pre_publish_cleanup_authority_with_quiescence(
                &progress,
                current_quiescence.clone(),
                PrePublishCleanupObservation::new(Vec::new()).expect("empty cleanup"),
                settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Held),
            ))
            .expect("current epoch retained proof");
        assert_eq!(progress.recovery_generation(), 2);
        assert_eq!(
            progress.disposition(),
            FullProgressDisposition::CancelRequestedBeforePublish
        );
        progress
            .finish_pre_publish_cancellation(pre_publish_cleanup_authority_with_quiescence(
                &progress,
                current_quiescence,
                PrePublishCleanupObservation::new(Vec::new()).expect("empty cleanup"),
                settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Released),
            ))
            .expect("current epoch release");
        assert_eq!(
            progress.disposition(),
            FullProgressDisposition::CancelledBeforePublish
        );
    }

    #[test]
    fn prepublish_cleanup_requires_recovery_absence_for_recovery_claims_and_allows_held_rows() {
        let mut progress = copy_progress_with_two_admission_claims();
        apply_role(&mut progress, 0, WorkUnitRole::SourceRead);
        apply_role(&mut progress, 0, WorkUnitRole::Prepare);
        progress
            .request_cancellation(CancellationRequestObservation::new(402).expect("cancel"))
            .expect("prepublication cancellation");

        let held_cleanup = PrePublishCleanupObservation::new(vec![PrePublishActionCleanup::new(
            0,
            Some(PrePublishFinalClaimObservation::NoFinalClaim),
            Some(PrePublishOriginalStateObservation::Preserved),
            Some(PrePublishAbsenceObservation::Absent),
            Some(PrePublishAbsenceObservation::Preserved),
        )])
        .expect("held cleanup");
        progress
            .finish_pre_publish_cancellation(pre_publish_cleanup_authority(
                &progress,
                held_cleanup,
                settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Held),
            ))
            .expect("retain held recovery observation");
        let before_release = progress.clone();
        assert_eq!(
            progress.finish_pre_publish_cancellation(pre_publish_cleanup_authority(
                &progress,
                progress
                    .pre_publish_cancellation()
                    .expect("held observation")
                    .cleanup()
                    .clone(),
                settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Released),
            )),
            Err(
                WorkProgressError::PrePublishCancellationSettlementCleanupIncomplete {
                    allocation_class: FullCapacityAllocationClass::OperationRecoveryPayload,
                }
            )
        );
        assert_eq!(progress, before_release);

        let complete_cleanup =
            PrePublishCleanupObservation::new(vec![PrePublishActionCleanup::new(
                0,
                Some(PrePublishFinalClaimObservation::NoFinalClaim),
                Some(PrePublishOriginalStateObservation::Preserved),
                Some(PrePublishAbsenceObservation::Absent),
                Some(PrePublishAbsenceObservation::Absent),
            )])
            .expect("complete cleanup");
        progress
            .finish_pre_publish_cancellation(pre_publish_cleanup_authority(
                &progress,
                complete_cleanup,
                settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Released),
            ))
            .expect("release staging and recovery claims");
        assert_eq!(
            progress.disposition(),
            FullProgressDisposition::CancelledBeforePublish
        );
    }

    #[test]
    fn prepublish_cleanup_enforces_create_folder_origin_policy() {
        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let parent = NamespaceDirectoryEndpoint::new(
            source,
            source_id.clone(),
            NamespaceDirectoryLocator::Root,
            NamespaceDirectoryExpectation::new(7, "root-a", Some(1)).expect("root"),
        )
        .expect("parent");
        let args = UniversalCommandArgs::Namespace(NamespaceCommandArgs::CreateFolder(
            CreateFolderIntentArgs::new(parent, NamespaceLeafName::new("created").expect("leaf"))
                .expect("create folder"),
        ));
        let plan = super::super::work_plan::derive_work_plan(&args, &participants)
            .expect("create-folder plan");
        let mut progress = WorkProgress::from_plan_with_admission_observations(
            OperationId::from_uuid(Uuid::from_u128(403)),
            &plan,
            &args,
            &participants,
            &admission_capacity_plan(),
            admission_claims(12),
        )
        .expect("create-folder progress");
        for role in [WorkUnitRole::Stage, WorkUnitRole::CopyValidated] {
            assert_eq!(
                progress
                    .plan
                    .units()
                    .iter()
                    .find(|unit| unit.key() == key_for(&progress, 0, role))
                    .expect("create-folder applicability")
                    .applicability(),
                WorkUnitApplicability::NotApplicable
            );
        }
        apply_role(&mut progress, 0, WorkUnitRole::SourceRead);
        apply_role(&mut progress, 0, WorkUnitRole::Prepare);
        progress
            .request_cancellation(CancellationRequestObservation::new(403).expect("cancel"))
            .expect("prepublication cancellation");
        assert!(
            progress
                .cancellation_effect_inventory()
                .expect("create-folder effects")
                .bundles()
                .is_empty()
        );

        let wrong_origin = PrePublishCleanupObservation::new(vec![PrePublishActionCleanup::new(
            0,
            Some(PrePublishFinalClaimObservation::NoFinalClaim),
            Some(PrePublishOriginalStateObservation::Preserved),
            None,
            None,
        )])
        .expect("wrong-origin cleanup");
        let before = progress.clone();
        assert_eq!(
            progress.finish_pre_publish_cancellation(pre_publish_cleanup_authority(
                &progress,
                wrong_origin,
                settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Held),
            )),
            Err(WorkProgressError::PrePublishCleanupOriginalStateMismatch { action_ordinal: 0 })
        );
        assert_eq!(progress, before);

        let valid_cleanup = PrePublishCleanupObservation::new(vec![PrePublishActionCleanup::new(
            0,
            Some(PrePublishFinalClaimObservation::NoFinalClaim),
            Some(PrePublishOriginalStateObservation::NoOriginalObject),
            None,
            None,
        )])
        .expect("valid create-folder cleanup");
        progress
            .finish_pre_publish_cancellation(pre_publish_cleanup_authority(
                &progress,
                valid_cleanup,
                settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Released),
            ))
            .expect("create-folder cancellation");
        assert_eq!(
            progress.disposition(),
            FullProgressDisposition::CancelledBeforePublish
        );
    }

    #[test]
    fn prepublish_cleanup_rejects_stale_bindings_and_non_prepublication_routes() {
        let mut progress = copy_progress_with_admission_claims();
        progress
            .request_cancellation(CancellationRequestObservation::new(404).expect("cancel"))
            .expect("prepublication cancellation");
        let cleanup = PrePublishCleanupObservation::new(Vec::new()).expect("empty cleanup");
        let settlement = settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Held);

        let mut wrong_operation =
            pre_publish_cleanup_authority(&progress, cleanup.clone(), settlement.clone());
        wrong_operation.operation_id = OperationId::from_uuid(Uuid::from_u128(405));
        let before = progress.clone();
        assert_eq!(
            progress.finish_pre_publish_cancellation(wrong_operation),
            Err(WorkProgressError::PrePublishCancellationAuthorityMismatch)
        );
        assert_eq!(progress, before);

        let mut wrong_epoch =
            pre_publish_cleanup_authority(&progress, cleanup.clone(), settlement.clone());
        wrong_epoch.execution_epoch = 1;
        assert_eq!(
            progress.finish_pre_publish_cancellation(wrong_epoch),
            Err(WorkProgressError::PrePublishCancellationEpochMismatch {
                expected: 0,
                actual: 1,
            })
        );
        assert_eq!(progress, before);

        let mut wrong_generation =
            pre_publish_cleanup_authority(&progress, cleanup.clone(), settlement.clone());
        wrong_generation.expected_recovery_generation = 1;
        assert_eq!(
            progress.finish_pre_publish_cancellation(wrong_generation),
            Err(
                WorkProgressError::PrePublishCancellationRecoveryGenerationMismatch {
                    expected: 0,
                    actual: 1,
                }
            )
        );
        assert_eq!(progress, before);

        let bad_row = PrePublishCleanupObservation::new(vec![PrePublishActionCleanup::new(
            1, None, None, None, None,
        )])
        .expect("bounded unknown action row");
        assert!(matches!(
            progress.finish_pre_publish_cancellation(pre_publish_cleanup_authority(
                &progress,
                bad_row,
                settlement.clone(),
            )),
            Err(WorkProgressError::PrePublishCleanupActionSetMismatch { action_ordinal: 1 })
        ));
        assert_eq!(progress, before);

        let mut published = copy_progress_with_admission_claims();
        publish_prefix(&mut published);
        published
            .request_cancellation(CancellationRequestObservation::new(406).expect("cancel"))
            .expect("postpublication cancellation");
        let published_before = published.clone();
        let published_inventory = published
            .cancellation_inventory
            .as_ref()
            .expect("published inventory")
            .clone();
        assert_eq!(
            published.finish_pre_publish_cancellation(VerifiedPrePublishCancellation {
                operation_id: published.operation_id,
                plan: published.plan.clone(),
                execution_epoch: published.execution_epoch,
                cancellation_inventory: published_inventory,
                expected_recovery_generation: 0,
                cleanup: PrePublishCleanupObservation::new(Vec::new()).expect("cleanup"),
                unstarted_quiescence: PrePublishUnstartedQuiescenceObservation::new(Vec::new())
                    .expect("quiescence"),
                settlement: settlement_inventory(
                    &published,
                    AdmissionClaimSettlementOutcome::Held,
                ),
                dispatch_outcome: all_never_dispatch_record(&published),
            }),
            Err(WorkProgressError::PrePublishCancellationRouteInvalid {
                route: CancellationRoute::AfterPublish,
            })
        );
        assert_eq!(published, published_before);
    }

    #[test]
    fn prepublish_cleanup_preserves_settled_rows_while_held_rows_progress() {
        let mut progress = copy_progress_with_two_admission_claims();
        apply_role(&mut progress, 0, WorkUnitRole::SourceRead);
        apply_role(&mut progress, 0, WorkUnitRole::Prepare);
        progress
            .request_cancellation(CancellationRequestObservation::new(407).expect("cancel"))
            .expect("prepublication cancellation");
        let cleanup = PrePublishCleanupObservation::new(vec![PrePublishActionCleanup::new(
            0,
            Some(PrePublishFinalClaimObservation::NoFinalClaim),
            Some(PrePublishOriginalStateObservation::Preserved),
            Some(PrePublishAbsenceObservation::Absent),
            Some(PrePublishAbsenceObservation::Absent),
        )])
        .expect("complete cleanup");
        let partial_settlement = settlement_inventory_with_outcomes(
            &progress,
            &[
                AdmissionClaimSettlementOutcome::Released,
                AdmissionClaimSettlementOutcome::Held,
            ],
        );
        progress
            .finish_pre_publish_cancellation(pre_publish_cleanup_authority(
                &progress,
                cleanup.clone(),
                partial_settlement,
            ))
            .expect("retain partial settlement");
        assert_eq!(
            progress.disposition(),
            FullProgressDisposition::CancelRequestedBeforePublish
        );

        let settled_rewrite = settlement_inventory_with_outcomes(
            &progress,
            &[
                AdmissionClaimSettlementOutcome::Held,
                AdmissionClaimSettlementOutcome::Held,
            ],
        );
        let before_rewrite = progress.clone();
        assert_eq!(
            progress.finish_pre_publish_cancellation(pre_publish_cleanup_authority(
                &progress,
                cleanup.clone(),
                settled_rewrite,
            )),
            Err(WorkProgressError::PrePublishCancellationSettlementReplayConflict)
        );
        assert_eq!(progress, before_rewrite);

        let complete_settlement = settlement_inventory_with_outcomes(
            &progress,
            &[
                AdmissionClaimSettlementOutcome::Released,
                AdmissionClaimSettlementOutcome::Released,
            ],
        );
        progress
            .finish_pre_publish_cancellation(pre_publish_cleanup_authority(
                &progress,
                cleanup,
                complete_settlement,
            ))
            .expect("held claim becomes released");
        assert_eq!(
            progress.disposition(),
            FullProgressDisposition::CancelledBeforePublish
        );
    }

    #[test]
    fn source_reacquisition_advances_shared_fence_and_invalidates_old_epoch() {
        let mut progress = copy_progress(false);
        let source_key = key_for(&progress, 0, WorkUnitRole::SourceRead);
        let source_participant_id = source_key.participant_id();
        apply_role(&mut progress, 0, WorkUnitRole::SourceRead);
        for role in [
            WorkUnitRole::Prepare,
            WorkUnitRole::Stage,
            WorkUnitRole::CopyValidated,
            WorkUnitRole::Publish,
        ] {
            apply_role(&mut progress, 0, role);
        }
        let source_affect = key_for(&progress, 0, WorkUnitRole::SourceAffect);
        progress
            .apply_verified_completion(completion(
                &progress,
                source_affect,
                StoredStepEvidence::SourceCommitted {
                    before_revision: 7,
                    after: source_observation(&progress, source_affect, 8),
                },
            ))
            .expect("source commit establishes revision floor");
        apply_role(&mut progress, u8::MAX, WorkUnitRole::GlobalLibrary);
        apply_role(&mut progress, u8::MAX, WorkUnitRole::Harvest);
        let projection = key_for(&progress, u8::MAX, WorkUnitRole::Projection);
        progress
            .apply_verified_completion(completion(
                &progress,
                projection,
                evidence_for(&progress, projection, 8),
            ))
            .expect("projection records source-bound revision");
        let readiness = key_for(&progress, u8::MAX, WorkUnitRole::Readiness);
        let stale_scope_completion =
            completion(&progress, readiness, evidence_for(&progress, readiness, 8));
        let plan_before = progress.plan.clone();
        let steps_before = progress.steps.clone();

        progress
            .reacquire_source(source_reacquisition_authority(
                &progress,
                source_participant_id,
                8,
                2,
                0,
            ))
            .expect("source owner reacquisition");
        assert_eq!(progress.execution_epoch(), 1);
        assert_eq!(progress.plan, plan_before);
        assert_eq!(progress.steps, steps_before);
        assert_eq!(progress.disposition(), FullProgressDisposition::None);
        for key in [source_key, projection, readiness] {
            let fence = progress
                .scope_for(key)
                .expect("shared source scope")
                .source_fence()
                .expect("shared source fence");
            assert_eq!(fence.source_revision(), 8);
            assert_eq!(fence.lease_epoch(), 2);
        }
        let harvest = key_for(&progress, u8::MAX, WorkUnitRole::Harvest);
        assert!(
            progress
                .scope_for(harvest)
                .expect("harvest scope")
                .source_fence()
                .is_none()
        );

        assert!(matches!(
            progress.apply_verified_completion(stale_scope_completion),
            Err(WorkProgressError::ParticipantScopeMismatch { key }) if key == readiness
        ));
        let mut stale_epoch =
            completion(&progress, readiness, evidence_for(&progress, readiness, 8));
        stale_epoch.execution_epoch = 0;
        assert!(matches!(
            progress.apply_verified_completion(stale_epoch),
            Err(WorkProgressError::ExecutionEpochMismatch {
                expected: 1,
                actual: 0
            })
        ));
        progress
            .apply_verified_completion(completion(
                &progress,
                readiness,
                evidence_for(&progress, readiness, 8),
            ))
            .expect("completion issued in reacquired epoch");
    }

    #[test]
    fn source_reacquisition_updates_source_fenced_harvest_and_rejects_stale_guards() {
        let mut fenced_harvest = fenced_harvest_progress();
        let source_key = key_for(&fenced_harvest, 0, WorkUnitRole::SourceRead);
        let source_participant_id = source_key.participant_id();
        let harvest_key = key_for(&fenced_harvest, u8::MAX, WorkUnitRole::Harvest);
        fenced_harvest
            .reacquire_source(source_reacquisition_authority(
                &fenced_harvest,
                source_participant_id,
                7,
                2,
                0,
            ))
            .expect("source owner reacquisition");
        for key in [source_key, harvest_key] {
            let fence = fenced_harvest
                .scope_for(key)
                .expect("shared source scope")
                .source_fence()
                .expect("fenced source scope");
            assert_eq!(fence.source_revision(), 7);
            assert_eq!(fence.lease_epoch(), 2);
        }

        let mut progress = copy_progress(false);
        let source_key = key_for(&progress, 0, WorkUnitRole::SourceRead);
        let target = source_key.participant_id();
        let before = progress.clone();
        let source_id = progress
            .scope_for(source_key)
            .expect("source scope")
            .source_fence()
            .expect("source fence")
            .source_id()
            .clone();
        for authority in [
            source_reacquisition_authority_with_identity(
                &progress,
                target,
                SourceId::from_string("other-source"),
                "root-a",
                "db-a",
                1,
                7,
                2,
                0,
            ),
            source_reacquisition_authority_with_identity(
                &progress,
                target,
                source_id.clone(),
                "different-root",
                "db-a",
                1,
                7,
                2,
                0,
            ),
            source_reacquisition_authority_with_identity(
                &progress,
                target,
                source_id.clone(),
                "root-a",
                "db-a",
                2,
                7,
                2,
                0,
            ),
        ] {
            assert!(matches!(
                progress.reacquire_source(authority),
                Err(WorkProgressError::SourceReacquisitionIdentityMismatch)
            ));
            assert_eq!(progress, before);
        }
        assert!(matches!(
            progress.reacquire_source(source_reacquisition_authority(&progress, target, 7, 1, 0,)),
            Err(WorkProgressError::SourceReacquisitionLeaseRegressed {
                observed: 1,
                current: 1
            })
        ));
        assert_eq!(progress, before);
        assert!(matches!(
            progress.reacquire_source(source_reacquisition_authority(&progress, target, 6, 2, 0,)),
            Err(WorkProgressError::SourceReacquisitionRevisionRegressed {
                observed: 6,
                minimum: 7
            })
        ));
        assert_eq!(progress, before);
        assert!(matches!(
            progress.reacquire_source(source_reacquisition_authority(&progress, target, 7, 2, 1,)),
            Err(WorkProgressError::SourceReacquisitionEpochMismatch {
                expected: 0,
                actual: 1
            })
        ));
        assert_eq!(progress, before);

        let old_scope = progress
            .scope_for(source_key)
            .expect("source scope")
            .clone();
        let first_authority = source_reacquisition_authority(&progress, target, 7, 2, 0);
        progress
            .reacquire_source(first_authority)
            .expect("first source reacquisition");
        let after_first = progress.clone();
        let old_fence = old_scope.source_fence().expect("old source fence").clone();
        let stale_authority = VerifiedSourceReacquisition {
            operation_id: progress.operation_id,
            plan: progress.plan.clone(),
            execution_epoch: 1,
            target_participant_id: target,
            old_scope,
            new_fence: StoredSourceFence::new(
                old_fence.source_id().clone(),
                old_fence.root_identity(),
                old_fence.database_identity(),
                old_fence.lifecycle_generation(),
                7,
                3,
            )
            .expect("stale authority fence"),
        };
        assert!(matches!(
            progress.reacquire_source(stale_authority),
            Err(WorkProgressError::SourceReacquisitionAuthorityMismatch)
        ));
        assert_eq!(progress, after_first);
    }

    #[test]
    fn source_reacquisition_preserves_retry_audit_and_cancellation_dispositions() {
        let observation = RetryObservation::new(
            1,
            0,
            1,
            RetryLeaseId::new("reacquire-retry").expect("retry lease"),
            10,
            RetryErrorClass::TransientContention,
            "busy",
            None,
        )
        .expect("retry observation");
        let mut retry = copy_progress(false);
        let target = key_for(&retry, 0, WorkUnitRole::SourceRead).participant_id();
        retry
            .request_retry(observation.clone())
            .expect("request retry");
        retry
            .reacquire_source(source_reacquisition_authority(&retry, target, 7, 2, 0))
            .expect("reacquire while retry pending");
        assert_eq!(retry.execution_epoch(), 1);
        assert_eq!(retry.disposition(), FullProgressDisposition::RetryPending);
        assert_eq!(retry.retry_observation(), Some(&observation));

        let mut audit = copy_progress(false);
        publish_prefix(&mut audit);
        let target = key_for(&audit, 0, WorkUnitRole::SourceRead).participant_id();
        let audit_observation = ProgressAuditObservation::new(ProgressAuditReason::RevisionGap);
        audit
            .require_audit(audit_observation.clone())
            .expect("require audit");
        audit
            .reacquire_source(source_reacquisition_authority(&audit, target, 7, 2, 0))
            .expect("reacquire while audit required");
        assert_eq!(audit.execution_epoch(), 1);
        assert_eq!(audit.disposition(), FullProgressDisposition::AuditRequired);
        assert_eq!(audit.audit_observation(), Some(&audit_observation));

        let mut cancelled = copy_progress_with_admission_claims();
        let target = key_for(&cancelled, 0, WorkUnitRole::SourceRead).participant_id();
        let cancellation = CancellationRequestObservation::new(77).expect("cancel request");
        cancelled
            .request_cancellation(cancellation)
            .expect("request cancellation");
        cancelled
            .reacquire_source(source_reacquisition_authority(&cancelled, target, 7, 2, 0))
            .expect("reacquire while cancellation requested");
        assert_eq!(cancelled.execution_epoch(), 1);
        assert_eq!(
            cancelled.disposition(),
            FullProgressDisposition::CancelRequestedBeforePublish
        );
        assert_eq!(cancelled.cancellation_request(), Some(cancellation));

        let mut terminal = copy_progress_with_admission_claims();
        complete_required(&mut terminal);
        let settlement = settlement_inventory(&terminal, AdmissionClaimSettlementOutcome::Released);
        terminal
            .apply_verified_claim_settlement(settlement_authority(&terminal, settlement))
            .expect("settle terminal progress");
        let target = key_for(&terminal, 0, WorkUnitRole::SourceRead).participant_id();
        let before_terminal = terminal.clone();
        assert!(matches!(
            terminal.reacquire_source(source_reacquisition_authority(&terminal, target, 7, 2, 0)),
            Err(WorkProgressError::TerminalTransitionForbidden)
        ));
        assert_eq!(terminal, before_terminal);
    }

    #[test]
    fn audit_overlay_requires_publication_and_validates_bounded_targets() {
        let mut prepublication = copy_progress(false);
        let before_prepublication = prepublication.clone();
        assert!(matches!(
            prepublication.require_audit(ProgressAuditObservation::new(
                ProgressAuditReason::UncertainEvidence
            )),
            Err(WorkProgressError::AuditPhaseInvalid)
        ));
        assert_eq!(prepublication, before_prepublication);

        let mut progress = copy_progress(false);
        publish_prefix(&mut progress);
        let first_key = key_for(&progress, 0, WorkUnitRole::SourceAffect);
        let second_key = key_for(&progress, u8::MAX, WorkUnitRole::GlobalLibrary);
        let mut unsorted = vec![first_key, second_key];
        unsorted.sort_unstable();
        let targeted = ProgressAuditObservation::for_target(
            ProgressAuditReason::LateCollision,
            AuditTarget::steps(vec![first_key]).expect("single target"),
        )
        .expect("targeted audit");
        progress
            .require_audit(targeted.clone())
            .expect("targeted audit request");
        assert_eq!(progress.audit_generation(), 1);
        assert_eq!(progress.audit_observation(), Some(&targeted));

        let before_exact_replay = progress.clone();
        progress
            .require_audit(targeted.clone())
            .expect("exact audit replay");
        assert_eq!(progress, before_exact_replay);

        let conflicting = ProgressAuditObservation::for_target(
            ProgressAuditReason::LateCollision,
            AuditTarget::steps(vec![second_key]).expect("different target"),
        )
        .expect("conflicting audit");
        let before_conflict = progress.clone();
        assert!(matches!(
            progress.require_audit(conflicting),
            Err(WorkProgressError::AuditReplayConflict)
        ));
        assert_eq!(progress, before_conflict);

        let mut invalid = copy_progress(false);
        publish_prefix(&mut invalid);
        let empty = ProgressAuditObservation {
            reason: ProgressAuditReason::UncertainEvidence,
            target: AuditTarget::Steps(Vec::new()),
        };
        let before_invalid = invalid.clone();
        assert!(matches!(
            invalid.require_audit(empty),
            Err(WorkProgressError::AuditTarget(
                AuditTargetValidationError::Empty
            ))
        ));
        assert_eq!(invalid, before_invalid);

        let duplicate = ProgressAuditObservation {
            reason: ProgressAuditReason::UncertainEvidence,
            target: AuditTarget::Steps(vec![first_key, first_key]),
        };
        assert!(matches!(
            invalid.require_audit(duplicate),
            Err(WorkProgressError::AuditTarget(
                AuditTargetValidationError::NotSortedUnique
            ))
        ));
        assert_eq!(invalid, before_invalid);

        let reversed = ProgressAuditObservation {
            reason: ProgressAuditReason::UncertainEvidence,
            target: AuditTarget::Steps(vec![unsorted[1], unsorted[0]]),
        };
        assert!(matches!(
            invalid.require_audit(reversed),
            Err(WorkProgressError::AuditTarget(
                AuditTargetValidationError::NotSortedUnique
            ))
        ));
        assert_eq!(invalid, before_invalid);

        let oversized = ProgressAuditObservation {
            reason: ProgressAuditReason::UncertainEvidence,
            target: AuditTarget::Steps(vec![first_key; MAX_WORK_PLAN_UNITS + 1]),
        };
        assert!(matches!(
            invalid.require_audit(oversized),
            Err(WorkProgressError::AuditTarget(
                AuditTargetValidationError::TooMany { .. }
            ))
        ));
        assert_eq!(invalid, before_invalid);

        let unknown_key = WorkUnitKey::new(
            first_key.action_ordinal(),
            u16::MAX,
            first_key.participant_id(),
            first_key.role(),
        );
        let unknown = ProgressAuditObservation::for_target(
            ProgressAuditReason::UncertainEvidence,
            AuditTarget::steps(vec![unknown_key]).expect("unknown target shape"),
        )
        .expect("unknown target observation");
        assert!(matches!(
            invalid.require_audit(unknown),
            Err(WorkProgressError::AuditTarget(
                AuditTargetValidationError::UnknownWorkUnit { key }
            )) if key == unknown_key
        ));
        assert_eq!(invalid, before_invalid);
    }

    #[test]
    fn audit_resolution_restores_holds_and_rejects_stale_or_changed_authority() {
        let mut progress = copy_progress(false);
        publish_prefix(&mut progress);
        let first_key = key_for(&progress, 0, WorkUnitRole::SourceAffect);
        let second_key = key_for(&progress, u8::MAX, WorkUnitRole::GlobalLibrary);
        let observation = ProgressAuditObservation::for_target(
            ProgressAuditReason::RevisionGap,
            AuditTarget::steps(vec![first_key]).expect("target"),
        )
        .expect("audit observation");
        progress
            .require_audit(observation.clone())
            .expect("require audit");
        let steps_before = progress.steps.clone();

        let mut wrong_authority = audit_resolution_authority(&progress, 1, 0);
        wrong_authority.operation_id = OperationId::from_uuid(Uuid::from_u128(106));
        assert!(matches!(
            progress.resolve_audit(wrong_authority),
            Err(WorkProgressError::AuditResolutionAuthorityMismatch)
        ));
        assert_eq!(progress.steps, steps_before);

        let stale_resolution = audit_resolution_authority(&progress, 1, 0);
        progress
            .resolve_audit(audit_resolution_authority(&progress, 1, 0))
            .expect("resolve audit");
        assert_eq!(progress.disposition(), FullProgressDisposition::None);
        assert!(progress.audit_observation().is_none());
        assert_eq!(progress.audit_generation(), 1);
        assert_eq!(progress.steps, steps_before);

        progress
            .require_audit(observation)
            .expect("reopen same audit observation");
        assert_eq!(progress.audit_generation(), 2);
        assert!(matches!(
            progress.resolve_audit(stale_resolution),
            Err(WorkProgressError::AuditResolutionGenerationMismatch {
                expected: 2,
                actual: 1
            })
        ));
        assert_eq!(
            progress.disposition(),
            FullProgressDisposition::AuditRequired
        );

        let mut retry = copy_progress(false);
        publish_prefix(&mut retry);
        let retry_observation = RetryObservation::new(
            1,
            0,
            1,
            RetryLeaseId::new("audit-hold-retry").expect("retry lease"),
            10,
            RetryErrorClass::TransientContention,
            "busy",
            None,
        )
        .expect("retry observation");
        retry
            .request_retry(retry_observation.clone())
            .expect("request retry");
        let retry_audit = ProgressAuditObservation::new(ProgressAuditReason::UncertainEvidence);
        retry.require_audit(retry_audit).expect("audit over retry");
        assert!(matches!(
            retry.request_retry(retry_observation.clone()),
            Err(WorkProgressError::ProgressSuspended {
                disposition: FullProgressDisposition::AuditRequired
            })
        ));
        retry
            .resolve_audit(audit_resolution_authority(&retry, 1, 0))
            .expect("resolve retry audit");
        assert_eq!(retry.disposition(), FullProgressDisposition::RetryPending);
        assert_eq!(retry.retry_observation(), Some(&retry_observation));
        retry
            .resume_retry(VerifiedRetryResume {
                operation_id: retry.operation_id,
                plan: retry.plan.clone(),
                retry_observation,
                now_unix_ms: 1,
                expected_execution_epoch: 0,
            })
            .expect("resume restored retry separately");
        assert_eq!(retry.disposition(), FullProgressDisposition::None);

        let mut cancelled = copy_progress_with_admission_claims();
        publish_prefix(&mut cancelled);
        let cancellation = CancellationRequestObservation::new(88).expect("cancel request");
        cancelled
            .require_audit(ProgressAuditObservation::new(
                ProgressAuditReason::LateCollision,
            ))
            .expect("audit before cancellation");
        cancelled
            .request_cancellation(cancellation)
            .expect("record cancellation under audit");
        assert_eq!(
            cancelled.disposition(),
            FullProgressDisposition::AuditRequired
        );
        assert_eq!(cancelled.cancellation_request(), Some(cancellation));
        cancelled
            .request_cancellation(cancellation)
            .expect("idempotent cancellation under audit");
        assert!(matches!(
            cancelled.request_cancellation(CancellationRequestObservation::new(89).unwrap()),
            Err(WorkProgressError::CancellationReplayConflict)
        ));
        cancelled
            .resolve_audit(audit_resolution_authority(&cancelled, 1, 0))
            .expect("resolve cancellation audit");
        assert_eq!(
            cancelled.disposition(),
            FullProgressDisposition::CancelRequestedAfterPublish
        );

        let _ = second_key;
    }

    #[test]
    fn audit_resolution_epoch_is_bound_to_source_reacquisition() {
        let mut progress = copy_progress(false);
        publish_prefix(&mut progress);
        progress
            .require_audit(ProgressAuditObservation::new(
                ProgressAuditReason::RevisionGap,
            ))
            .expect("require audit");
        let stale_resolution = audit_resolution_authority(&progress, 1, 0);
        let target = key_for(&progress, 0, WorkUnitRole::SourceRead).participant_id();
        progress
            .reacquire_source(source_reacquisition_authority(&progress, target, 7, 2, 0))
            .expect("source reacquisition preserves audit");
        assert_eq!(progress.execution_epoch(), 1);
        assert_eq!(
            progress.disposition(),
            FullProgressDisposition::AuditRequired
        );
        assert!(matches!(
            progress.resolve_audit(stale_resolution),
            Err(WorkProgressError::AuditResolutionEpochMismatch {
                expected: 1,
                actual: 0
            })
        ));
        assert_eq!(
            progress.disposition(),
            FullProgressDisposition::AuditRequired
        );
    }

    #[test]
    fn accepted_after_effect_reconciliation_is_nonterminal_and_blocks_prepublish_bypass() {
        let mut progress = copy_progress_with_admission_claims();
        progress
            .request_cancellation(CancellationRequestObservation::new(1401).expect("cancel"))
            .expect("request cancellation");
        let dispatch = all_never_dispatch_record(&progress);
        let (recovery, source, coordinator) = empty_after_effect_inputs(&progress, dispatch);
        let before_steps = progress.steps().to_vec();
        let before_inventory = progress.cancellation_inventory().cloned();
        let before_phase = progress.phase();
        let before_disposition = progress.disposition();

        progress
            .accept_after_effect_reconciliation(after_effect_authority(
                &progress,
                recovery.clone(),
                source.clone(),
                coordinator.clone(),
                progress.recovery_generation(),
            ))
            .expect("accept nonterminal after-effect state");
        assert_eq!(progress.recovery_generation(), 1);
        assert_eq!(progress.phase(), before_phase);
        assert_eq!(progress.disposition(), before_disposition);
        assert_eq!(progress.steps(), before_steps.as_slice());
        assert_eq!(progress.cancellation_inventory(), before_inventory.as_ref());
        assert_eq!(
            progress
                .after_effect_reconciliation()
                .expect("accepted state")
                .recovery_inventory(),
            &recovery
        );

        progress
            .accept_after_effect_reconciliation(after_effect_authority(
                &progress,
                recovery,
                source,
                coordinator,
                progress.recovery_generation(),
            ))
            .expect("exact acknowledgement-loss replay");
        assert_eq!(progress.recovery_generation(), 1);

        let before_bypass = progress.clone();
        assert_eq!(
            progress.finish_pre_publish_cancellation(pre_publish_cleanup_authority(
                &progress,
                PrePublishCleanupObservation::new(Vec::new()).expect("cleanup"),
                settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Held),
            )),
            Err(WorkProgressError::PrePublishCancellationAfterEffectReconciliationActive)
        );
        assert_eq!(progress, before_bypass);
    }

    #[test]
    fn after_effect_reconciliation_accepts_after_publish_and_observed_effect_routes() {
        let mut published = copy_progress_with_admission_claims();
        publish_prefix(&mut published);
        assert_after_effect_acceptance_preserves_frozen_state(
            published,
            1410,
            CancellationRoute::AfterPublish,
        );

        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let mut observed = claimed_progress(
            participants,
            retained_resolution_args(source, source_id, false),
            1411,
        );
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::Prepare,
            WorkUnitRole::SelectedRetainedObjectDisposition,
        ] {
            apply_role(&mut observed, 0, role);
        }
        assert_after_effect_acceptance_preserves_frozen_state(
            observed,
            1411,
            CancellationRoute::AfterObservedEffect,
        );
    }

    #[test]
    fn after_effect_reconciliation_rejects_malformed_headers_and_stale_authority_atomically() {
        let mut progress = copy_progress_with_admission_claims();
        progress
            .request_cancellation(CancellationRequestObservation::new(1412).expect("cancel"))
            .expect("request cancellation");
        let dispatch = all_never_dispatch_record(&progress);
        let (recovery, source, coordinator) = empty_after_effect_inputs(&progress, dispatch);
        let authority = after_effect_authority(
            &progress,
            recovery.clone(),
            source.clone(),
            coordinator.clone(),
            progress.recovery_generation(),
        );

        let before_wrong_operation = progress.clone();
        let mut wrong_operation = authority.clone();
        wrong_operation.operation_id = OperationId::from_uuid(Uuid::from_u128(1413));
        assert_eq!(
            progress.accept_after_effect_reconciliation(wrong_operation),
            Err(WorkProgressError::AfterEffectReconciliationAuthorityMismatch)
        );
        assert_eq!(progress, before_wrong_operation);

        let before_wrong_epoch = progress.clone();
        let mut wrong_epoch = authority.clone();
        wrong_epoch.execution_epoch = progress.execution_epoch() + 1;
        assert_eq!(
            progress.accept_after_effect_reconciliation(wrong_epoch),
            Err(WorkProgressError::AfterEffectReconciliationEpochMismatch {
                expected: 0,
                actual: 1,
            })
        );
        assert_eq!(progress, before_wrong_epoch);

        let mut other = copy_progress_with_two_admission_claims();
        other
            .request_cancellation(CancellationRequestObservation::new(1414).expect("cancel"))
            .expect("other cancellation");
        let other_dispatch = all_never_dispatch_record(&other);
        let (other_recovery, other_source, other_coordinator) =
            empty_after_effect_inputs(&other, other_dispatch);
        let before_wrong_lineage = progress.clone();
        let mut wrong_lineage = authority.clone();
        wrong_lineage.recovery_inventory = other_recovery;
        wrong_lineage.source_closure_observation = other_source;
        wrong_lineage.coordinator_closure_observation = other_coordinator;
        assert!(
            progress
                .accept_after_effect_reconciliation(wrong_lineage)
                .is_err()
        );
        assert_eq!(progress, before_wrong_lineage);

        progress
            .accept_after_effect_reconciliation(authority.clone())
            .expect("accept valid state");
        let source_key = key_for(&progress, 0, WorkUnitRole::SourceRead);
        progress
            .reacquire_source(source_reacquisition_authority(
                &progress,
                source_key.participant_id(),
                7,
                2,
                progress.execution_epoch(),
            ))
            .expect("reacquire source");
        let before_stale_authority = progress.clone();
        assert_eq!(
            progress.accept_after_effect_reconciliation(authority),
            Err(WorkProgressError::AfterEffectReconciliationEpochMismatch {
                expected: 1,
                actual: 0,
            })
        );
        assert_eq!(progress, before_stale_authority);
    }

    #[test]
    fn after_effect_reconciliation_preserves_audit_hold() {
        let mut progress = copy_progress_with_admission_claims();
        publish_prefix(&mut progress);
        progress
            .require_audit(ProgressAuditObservation::new(
                ProgressAuditReason::LateCollision,
            ))
            .expect("require audit");
        progress
            .request_cancellation(CancellationRequestObservation::new(1406).expect("cancel"))
            .expect("request cancellation under audit");
        let dispatch = all_never_dispatch_record(&progress);
        let (recovery, source, coordinator) = empty_after_effect_inputs(&progress, dispatch);
        let before = progress.clone();
        assert_eq!(
            progress.accept_after_effect_reconciliation(after_effect_authority(
                &progress,
                recovery,
                source,
                coordinator,
                progress.recovery_generation(),
            )),
            Err(WorkProgressError::AfterEffectReconciliationAuditActive)
        );
        assert_eq!(progress, before);
    }

    #[test]
    fn after_effect_generation_replay_at_max_is_stable_and_new_state_overflows() {
        let mut progress = copy_progress_with_admission_claims();
        progress
            .request_cancellation(CancellationRequestObservation::new(1403).expect("cancel"))
            .expect("request cancellation");
        let dispatch = all_never_dispatch_record(&progress);
        let (recovery, source, coordinator) = empty_after_effect_inputs(&progress, dispatch);
        progress
            .accept_after_effect_reconciliation(after_effect_authority(
                &progress,
                recovery.clone(),
                source.clone(),
                coordinator.clone(),
                progress.recovery_generation(),
            ))
            .expect("accept initial state");
        progress.recovery_generation = u64::MAX;
        progress
            .after_effect_reconciliation
            .as_mut()
            .expect("accepted state")
            .recovery_generation = u64::MAX;

        progress
            .accept_after_effect_reconciliation(after_effect_authority(
                &progress,
                recovery.clone(),
                source.clone(),
                coordinator.clone(),
                u64::MAX,
            ))
            .expect("maximum-generation exact replay remains idempotent");

        let source_manifest = source.manifest().clone();
        let changed_source = CancellationSourceClosureObservationRecord::new(
            source_manifest.clone(),
            progress.execution_epoch(),
            2,
            Vec::new(),
        )
        .expect("changed empty source wrapper");
        let changed_coordinator_manifest = progress
            .cancellation_coordinator_closure_manifest(
                &changed_source,
                &recovery,
                recovery.dispatch_outcome(),
            )
            .expect("changed coordinator manifest");
        let changed_coordinator =
            coordinator_closure_record(changed_coordinator_manifest, progress.execution_epoch(), 2);
        let before_overflow = progress.clone();
        assert_eq!(
            progress.accept_after_effect_reconciliation(after_effect_authority(
                &progress,
                recovery,
                changed_source,
                changed_coordinator,
                u64::MAX,
            )),
            Err(WorkProgressError::AfterEffectReconciliationGenerationOverflow)
        );
        assert_eq!(progress, before_overflow);
    }

    #[test]
    fn after_effect_phase_gates_reject_uncertain_dispatch_and_partial_source_closure() {
        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let mut progress =
            claimed_progress(participants, history_folder_args(source, source_id), 1404);
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::Prepare,
            WorkUnitRole::EnsureContainedParentDirectories,
            WorkUnitRole::QualifiedRelocation,
            WorkUnitRole::OriginAbsence,
            WorkUnitRole::SourceAffect,
        ] {
            apply_step(&mut progress, 0, 0, role);
        }
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::Prepare,
            WorkUnitRole::EnsureContainedParentDirectories,
        ] {
            apply_step(&mut progress, 0, 1, role);
        }
        progress
            .request_cancellation(CancellationRequestObservation::new(1404).expect("cancel"))
            .expect("request cancellation");
        let dispatch_manifest = progress
            .cancellation_dispatch_manifest()
            .expect("dispatch manifest");
        let late_key = key_for_step(&progress, 0, 1, WorkUnitRole::QualifiedRelocation);
        let late_observation = (
            late_key,
            evidence_for(&progress, late_key, 7),
            CompletionContext::new(
                progress.execution_epoch(),
                progress.scope_for(late_key).expect("late scope").clone(),
            ),
        );
        let uncertain_dispatch = CancellationDispatchOutcomeRecord::new(
            dispatch_manifest.clone(),
            progress.execution_epoch(),
            1,
            dispatch_rows_with_observed(&dispatch_manifest, &[late_observation.clone()]),
        )
        .expect("uncertain dispatch record");
        let uncertain_recovery = progress
            .cancellation_effect_recovery_inventory(&uncertain_dispatch)
            .expect("uncertain recovery inventory");
        let uncertain_source_manifest = progress
            .cancellation_source_closure_manifest(&uncertain_recovery, &uncertain_dispatch)
            .expect("uncertain source manifest");
        let source_key = uncertain_source_manifest.keys()[0];
        let source_observed = CancellationSourceClosureObservationRecord::new(
            uncertain_source_manifest.clone(),
            progress.execution_epoch(),
            1,
            vec![CancellationSourceClosureObservationRow::new(
                source_key,
                CancellationSourceClosureOutcome::Observed {
                    evidence: evidence_for(&progress, source_key, 8),
                    context: CompletionContext::new(
                        progress.execution_epoch(),
                        progress
                            .scope_for(source_key)
                            .expect("source scope")
                            .clone(),
                    ),
                },
            )],
        )
        .expect("source observation");
        let uncertain_coordinator_manifest = progress
            .cancellation_coordinator_closure_manifest(
                &source_observed,
                &uncertain_recovery,
                &uncertain_dispatch,
            )
            .expect("uncertain coordinator manifest");
        let uncertain_coordinator = coordinator_closure_record(
            uncertain_coordinator_manifest,
            progress.execution_epoch(),
            1,
        );
        let before_uncertain = progress.clone();
        assert_eq!(
            progress.accept_after_effect_reconciliation(after_effect_authority(
                &progress,
                uncertain_recovery,
                source_observed.clone(),
                uncertain_coordinator,
                progress.recovery_generation(),
            )),
            Err(WorkProgressError::AfterEffectReconciliationDispatchIndeterminate)
        );
        assert_eq!(progress, before_uncertain);

        let determinate_dispatch = CancellationDispatchOutcomeRecord::new(
            dispatch_manifest.clone(),
            progress.execution_epoch(),
            1,
            dispatch_rows_with_never_entered(&dispatch_manifest, &[late_observation]),
        )
        .expect("determinate dispatch record");
        let determinate_recovery = progress
            .cancellation_effect_recovery_inventory(&determinate_dispatch)
            .expect("determinate recovery inventory");
        let determinate_source_manifest = progress
            .cancellation_source_closure_manifest(&determinate_recovery, &determinate_dispatch)
            .expect("determinate source manifest");
        let source_observed = CancellationSourceClosureObservationRecord::new(
            determinate_source_manifest.clone(),
            progress.execution_epoch(),
            1,
            source_closure_rows(&determinate_source_manifest)
                .into_iter()
                .map(|row| {
                    CancellationSourceClosureObservationRow::new(
                        row.key(),
                        CancellationSourceClosureOutcome::Observed {
                            evidence: evidence_for(&progress, row.key(), 8),
                            context: CompletionContext::new(
                                progress.execution_epoch(),
                                progress.scope_for(row.key()).expect("source scope").clone(),
                            ),
                        },
                    )
                })
                .collect(),
        )
        .expect("determinate source observation");
        let determinate_coordinator_manifest = progress
            .cancellation_coordinator_closure_manifest(
                &source_observed,
                &determinate_recovery,
                &determinate_dispatch,
            )
            .expect("determinate coordinator manifest");
        let determinate_coordinator = coordinator_closure_record(
            determinate_coordinator_manifest.clone(),
            progress.execution_epoch(),
            1,
        );
        progress
            .accept_after_effect_reconciliation(after_effect_authority(
                &progress,
                determinate_recovery.clone(),
                source_observed,
                determinate_coordinator,
                progress.recovery_generation(),
            ))
            .expect("determinate dispatch with source receipts");

        let pending_source = CancellationSourceClosureObservationRecord::new(
            determinate_source_manifest.clone(),
            progress.execution_epoch(),
            2,
            source_closure_rows(&determinate_source_manifest),
        )
        .expect("pending source observation");
        let partial_coordinator_manifest = progress
            .cancellation_coordinator_closure_manifest(
                &pending_source,
                &determinate_recovery,
                &determinate_dispatch,
            )
            .expect("partial coordinator manifest");
        let coordinator_key = partial_coordinator_manifest.keys()[0];
        let partial_coordinator = CancellationCoordinatorClosureObservationRecord::new(
            partial_coordinator_manifest.clone(),
            progress.execution_epoch(),
            2,
            partial_coordinator_manifest
                .keys()
                .iter()
                .map(|key| {
                    let outcome = if *key == coordinator_key {
                        CancellationCoordinatorClosureOutcome::Observed {
                            evidence: evidence_for(&progress, *key, 8),
                            context: CompletionContext::new(
                                progress.execution_epoch(),
                                progress.scope_for(*key).expect("coordinator scope").clone(),
                            ),
                        }
                    } else {
                        CancellationCoordinatorClosureOutcome::Pending
                    };
                    CancellationCoordinatorClosureObservationRow::new(*key, outcome)
                })
                .collect(),
        )
        .expect("partial coordinator observation");
        let before_partial = progress.clone();
        assert_eq!(
            progress.accept_after_effect_reconciliation(after_effect_authority(
                &progress,
                determinate_recovery,
                pending_source,
                partial_coordinator,
                progress.recovery_generation(),
            )),
            Err(WorkProgressError::AfterEffectReconciliationSourceClosureIncomplete)
        );
        assert_eq!(progress, before_partial);
    }

    #[test]
    fn accepted_after_effect_source_receipts_raise_reacquisition_floor_and_replay_historically() {
        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let args = history_folder_args(source, source_id.clone());
        let mut progress = claimed_progress(participants.clone(), args.clone(), 1405);
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::Prepare,
            WorkUnitRole::EnsureContainedParentDirectories,
            WorkUnitRole::QualifiedRelocation,
            WorkUnitRole::OriginAbsence,
            WorkUnitRole::SourceAffect,
        ] {
            apply_step(&mut progress, 0, 0, role);
        }
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::Prepare,
            WorkUnitRole::EnsureContainedParentDirectories,
        ] {
            apply_step(&mut progress, 0, 1, role);
        }
        progress
            .request_cancellation(CancellationRequestObservation::new(1405).expect("cancel"))
            .expect("request cancellation");
        let dispatch_manifest = progress
            .cancellation_dispatch_manifest()
            .expect("dispatch manifest");
        let late_key = key_for_step(&progress, 0, 1, WorkUnitRole::QualifiedRelocation);
        let late_observation = (
            late_key,
            evidence_for(&progress, late_key, 7),
            CompletionContext::new(
                progress.execution_epoch(),
                progress.scope_for(late_key).expect("late scope").clone(),
            ),
        );
        let dispatch = CancellationDispatchOutcomeRecord::new(
            dispatch_manifest.clone(),
            progress.execution_epoch(),
            1,
            dispatch_rows_with_never_entered(&dispatch_manifest, &[late_observation]),
        )
        .expect("dispatch record");
        let recovery = progress
            .cancellation_effect_recovery_inventory(&dispatch)
            .expect("recovery inventory");
        let source_manifest = progress
            .cancellation_source_closure_manifest(&recovery, &dispatch)
            .expect("source manifest");
        let source_key = source_manifest.keys()[0];
        let pending_source = CancellationSourceClosureObservationRecord::new(
            source_manifest.clone(),
            progress.execution_epoch(),
            1,
            source_closure_rows(&source_manifest),
        )
        .expect("pending source");
        let coordinator_manifest = progress
            .cancellation_coordinator_closure_manifest(&pending_source, &recovery, &dispatch)
            .expect("coordinator manifest");
        let pending_coordinator =
            coordinator_closure_record(coordinator_manifest, progress.execution_epoch(), 1);
        progress
            .accept_after_effect_reconciliation(after_effect_authority(
                &progress,
                recovery.clone(),
                pending_source,
                pending_coordinator,
                progress.recovery_generation(),
            ))
            .expect("accept pending after-effect state");

        let historical_context = CompletionContext::new(
            progress.execution_epoch(),
            progress
                .scope_for(source_key)
                .expect("source scope")
                .clone(),
        );
        let historical_evidence = evidence_for(&progress, source_key, 8);
        let observed_source = CancellationSourceClosureObservationRecord::new(
            source_manifest.clone(),
            progress.execution_epoch(),
            2,
            vec![CancellationSourceClosureObservationRow::new(
                source_key,
                CancellationSourceClosureOutcome::Observed {
                    evidence: historical_evidence.clone(),
                    context: historical_context.clone(),
                },
            )],
        )
        .expect("observed source");
        let observed_coordinator_manifest = progress
            .cancellation_coordinator_closure_manifest(&observed_source, &recovery, &dispatch)
            .expect("observed coordinator manifest");
        let observed_coordinator = coordinator_closure_record(
            observed_coordinator_manifest,
            progress.execution_epoch(),
            2,
        );
        progress
            .accept_after_effect_reconciliation(after_effect_authority(
                &progress,
                recovery.clone(),
                observed_source,
                observed_coordinator,
                progress.recovery_generation(),
            ))
            .expect("accept observed source");
        progress
            .capture_full_progress_snapshot(&snapshot_intent_for(
                &progress,
                args.clone(),
                participants.clone(),
            ))
            .expect("snapshot with observed source receipt")
            .validate_base()
            .expect("snapshot validates observed source receipt");

        let source_snapshot = progress
            .capture_full_progress_snapshot(&snapshot_intent_for(
                &progress,
                args.clone(),
                participants.clone(),
            ))
            .expect("source receipt snapshot");
        let pending_source = CancellationSourceClosureObservationRecord::new(
            source_snapshot
                .after_effect_reconciliation()
                .expect("after-effect state")
                .source_closure_observation()
                .manifest()
                .clone(),
            progress.execution_epoch(),
            3,
            source_closure_rows(
                source_snapshot
                    .after_effect_reconciliation()
                    .expect("after-effect state")
                    .source_closure_observation()
                    .manifest(),
            ),
        )
        .expect("pending source receipt");
        let coordinator_manifest = progress
            .cancellation_coordinator_closure_manifest(
                &pending_source,
                source_snapshot
                    .after_effect_reconciliation()
                    .expect("after-effect state")
                    .recovery_inventory(),
                source_snapshot
                    .after_effect_reconciliation()
                    .expect("after-effect state")
                    .recovery_inventory()
                    .dispatch_outcome(),
            )
            .expect("coordinator manifest");
        let observed_coordinator_key = coordinator_manifest
            .keys()
            .first()
            .copied()
            .expect("coordinator closure key");
        let observed_coordinator = CancellationCoordinatorClosureObservationRecord::new(
            coordinator_manifest.clone(),
            progress.execution_epoch(),
            3,
            coordinator_manifest
                .keys()
                .iter()
                .map(|key| {
                    let outcome = if *key == observed_coordinator_key {
                        CancellationCoordinatorClosureOutcome::Observed {
                            evidence: evidence_for(&progress, *key, 9),
                            context: CompletionContext::new(
                                progress.execution_epoch(),
                                progress.scope_for(*key).expect("coordinator scope").clone(),
                            ),
                        }
                    } else {
                        CancellationCoordinatorClosureOutcome::Pending
                    };
                    CancellationCoordinatorClosureObservationRow::new(*key, outcome)
                })
                .collect(),
        )
        .expect("observed coordinator receipt");
        let accepted = source_snapshot
            .after_effect_reconciliation()
            .expect("after-effect state");
        let dispatch = accepted.recovery_inventory().dispatch_outcome();
        let never_entered_key = dispatch
            .rows()
            .iter()
            .find(|row| matches!(row.outcome(), CancellationDispatchOutcome::NeverEntered))
            .map(CancellationDispatchOutcomeRow::key)
            .expect("never-entered dispatch row");
        let uncertain_rows = dispatch
            .rows()
            .iter()
            .map(|row| {
                let outcome = if row.key() == never_entered_key {
                    CancellationDispatchOutcome::EntryUncertain
                } else {
                    row.outcome().clone()
                };
                CancellationDispatchOutcomeRow::new(row.key(), outcome)
            })
            .collect();
        let uncertain_dispatch = CancellationDispatchOutcomeRecord::new(
            dispatch.manifest().clone(),
            dispatch.observation_epoch(),
            dispatch.recovery_generation(),
            uncertain_rows,
        )
        .expect("uncertain dispatch");
        let uncertain_recovery = progress
            .cancellation_effect_recovery_inventory(&uncertain_dispatch)
            .expect("uncertain recovery");
        let uncertain_source_manifest = progress
            .cancellation_source_closure_manifest(&uncertain_recovery, &uncertain_dispatch)
            .expect("uncertain source manifest");
        let uncertain_source = CancellationSourceClosureObservationRecord::new(
            uncertain_source_manifest,
            accepted.source_closure_observation().observation_epoch(),
            accepted
                .source_closure_observation()
                .observation_generation()
                + 1,
            accepted.source_closure_observation().rows().to_vec(),
        )
        .expect("uncertain source");
        let uncertain_coordinator_manifest = progress
            .cancellation_coordinator_closure_manifest(
                &uncertain_source,
                &uncertain_recovery,
                &uncertain_dispatch,
            )
            .expect("uncertain coordinator manifest");
        let uncertain_coordinator = CancellationCoordinatorClosureObservationRecord::new(
            uncertain_coordinator_manifest,
            accepted
                .coordinator_closure_observation()
                .observation_epoch(),
            accepted
                .coordinator_closure_observation()
                .observation_generation()
                + 1,
            accepted.coordinator_closure_observation().rows().to_vec(),
        )
        .expect("uncertain coordinator");
        let uncertain_after_effect = AfterEffectReconciliationObservation::new(
            uncertain_recovery,
            uncertain_source,
            uncertain_coordinator,
            accepted.execution_epoch(),
            accepted.recovery_generation(),
        );
        let mut uncertain_snapshot =
            serde_json::to_value(&source_snapshot).expect("snapshot value");
        uncertain_snapshot["after_effect_reconciliation"] =
            serde_json::to_value(uncertain_after_effect).expect("after-effect value");
        let uncertain_snapshot: FullProgressSnapshotObservation =
            serde_json::from_value(uncertain_snapshot).expect("uncertain dispatch snapshot");
        let uncertain_error = uncertain_snapshot
            .validate_base()
            .expect_err("uncertain dispatch must fail");
        assert!(matches!(
            uncertain_error,
            FullProgressSnapshotObservationError::AfterEffectValidation {
                source: WorkProgressError::AfterEffectReconciliationDispatchIndeterminate,
            }
        ));

        let mut mismatched_coordinator =
            serde_json::to_value(&source_snapshot).expect("snapshot value");
        mismatched_coordinator["after_effect_reconciliation"]["coordinator_closure_observation"] =
            serde_json::to_value(&observed_coordinator).expect("coordinator value");
        let mismatched_coordinator: FullProgressSnapshotObservation =
            serde_json::from_value(mismatched_coordinator)
                .expect("mismatched coordinator snapshot");
        assert!(matches!(
            mismatched_coordinator.validate_base(),
            Err(
                FullProgressSnapshotObservationError::AfterEffectValidation {
                    source: WorkProgressError::CancellationCoordinatorClosureObservation(_),
                }
            )
        ));

        let mut partial_source = serde_json::to_value(&source_snapshot).expect("snapshot value");
        partial_source["after_effect_reconciliation"]["source_closure_observation"] =
            serde_json::to_value(&pending_source).expect("pending source value");
        partial_source["after_effect_reconciliation"]["coordinator_closure_observation"] =
            serde_json::to_value(&observed_coordinator).expect("coordinator value");
        let partial_source: FullProgressSnapshotObservation =
            serde_json::from_value(partial_source).expect("partial source snapshot");
        assert!(matches!(
            partial_source.validate_base(),
            Err(
                FullProgressSnapshotObservationError::AfterEffectValidation {
                    source: WorkProgressError::AfterEffectReconciliationSourceClosureIncomplete,
                }
            )
        ));

        let target = source_key.participant_id();
        let before_lower_reacquisition = progress.clone();
        assert_eq!(
            progress.reacquire_source(source_reacquisition_authority(
                &progress,
                target,
                7,
                2,
                progress.execution_epoch(),
            )),
            Err(WorkProgressError::SourceReacquisitionRevisionRegressed {
                observed: 7,
                minimum: 8,
            })
        );
        assert_eq!(progress, before_lower_reacquisition);
        progress
            .reacquire_source(source_reacquisition_authority(
                &progress,
                target,
                8,
                2,
                progress.execution_epoch(),
            ))
            .expect("reacquire at accepted historical floor");
        assert_eq!(progress.execution_epoch(), 1);

        let (late_evidence, late_context) = recovery
            .dispatch_outcome()
            .rows()
            .iter()
            .find(|row| row.key() == late_key)
            .and_then(|row| match row.outcome() {
                CancellationDispatchOutcome::ObservedAfterCancellation { evidence, context } => {
                    Some((evidence.clone(), context.clone()))
                }
                _ => None,
            })
            .expect("late dispatch evidence");
        let refreshed_manifest = recovery.dispatch_outcome().manifest().clone();
        let refreshed_dispatch = CancellationDispatchOutcomeRecord::new(
            refreshed_manifest.clone(),
            progress.execution_epoch(),
            3,
            dispatch_rows_with_never_entered(
                &refreshed_manifest,
                &[(late_key, late_evidence, late_context)],
            ),
        )
        .expect("refreshed dispatch");
        let refreshed_recovery = progress
            .cancellation_effect_recovery_inventory(&refreshed_dispatch)
            .expect("refreshed recovery");
        let refreshed_source_manifest = progress
            .cancellation_source_closure_manifest(&refreshed_recovery, &refreshed_dispatch)
            .expect("refreshed source manifest");
        let refreshed_source = CancellationSourceClosureObservationRecord::new(
            refreshed_source_manifest.clone(),
            progress.execution_epoch(),
            3,
            vec![CancellationSourceClosureObservationRow::new(
                source_key,
                CancellationSourceClosureOutcome::Observed {
                    evidence: historical_evidence.clone(),
                    context: historical_context.clone(),
                },
            )],
        )
        .expect("refreshed source");
        let refreshed_coordinator_manifest = progress
            .cancellation_coordinator_closure_manifest(
                &refreshed_source,
                &refreshed_recovery,
                &refreshed_dispatch,
            )
            .expect("refreshed coordinator manifest");
        let refreshed_coordinator = coordinator_closure_record(
            refreshed_coordinator_manifest,
            progress.execution_epoch(),
            3,
        );
        progress
            .accept_after_effect_reconciliation(after_effect_authority(
                &progress,
                refreshed_recovery,
                refreshed_source,
                refreshed_coordinator,
                progress.recovery_generation(),
            ))
            .expect("accept refreshed current-epoch headers with historical receipt");
        assert_eq!(progress.recovery_generation(), 3);
        assert_eq!(
            progress
                .after_effect_reconciliation()
                .expect("refreshed accepted state")
                .source_closure_observation()
                .rows()[0]
                .outcome(),
            &CancellationSourceClosureOutcome::Observed {
                evidence: historical_evidence,
                context: historical_context,
            }
        );
        progress
            .capture_full_progress_snapshot(&snapshot_intent_for(&progress, args, participants))
            .expect("reacquired historical source snapshot")
            .validate_base()
            .expect("reacquired historical source snapshot validates");
    }

    #[test]
    fn accepted_after_effect_coordinator_receipt_raises_reacquisition_floor_and_replays_historically()
     {
        let mut progress = fenced_harvest_progress_with_admission_claims();
        progress
            .request_cancellation(CancellationRequestObservation::new(1415).expect("cancel"))
            .expect("request cancellation");
        let dispatch_manifest = progress
            .cancellation_dispatch_manifest()
            .expect("dispatch manifest");
        let source_affect = key_for(&progress, 0, WorkUnitRole::SourceAffect);
        let harvest = key_for(&progress, u8::MAX, WorkUnitRole::Harvest);
        assert!(dispatch_manifest.keys().contains(&source_affect));
        assert!(dispatch_manifest.keys().contains(&harvest));
        assert!(
            progress
                .scope_for(harvest)
                .expect("harvest scope")
                .source_fence()
                .is_some()
        );
        let source_affect_evidence = StoredStepEvidence::SourceCommitted {
            before_revision: 8,
            after: source_observation(&progress, source_affect, 8),
        };
        let source_affect_context = CompletionContext::new(
            progress.execution_epoch(),
            progress
                .scope_for(source_affect)
                .expect("source-affect scope")
                .clone(),
        );
        let dispatch = CancellationDispatchOutcomeRecord::new(
            dispatch_manifest.clone(),
            progress.execution_epoch(),
            1,
            dispatch_rows_with_never_entered(
                &dispatch_manifest,
                &[(
                    source_affect,
                    source_affect_evidence.clone(),
                    source_affect_context.clone(),
                )],
            ),
        )
        .expect("dispatch record");
        let recovery = progress
            .cancellation_effect_recovery_inventory(&dispatch)
            .expect("recovery inventory");
        let source_manifest = progress
            .cancellation_source_closure_manifest(&recovery, &dispatch)
            .expect("source manifest");
        let source_pending = CancellationSourceClosureObservationRecord::new(
            source_manifest.clone(),
            progress.execution_epoch(),
            1,
            source_closure_rows(&source_manifest),
        )
        .expect("source record");
        let coordinator_manifest = progress
            .cancellation_coordinator_closure_manifest(&source_pending, &recovery, &dispatch)
            .expect("coordinator manifest");
        assert!(coordinator_manifest.keys().contains(&harvest));
        let coordinator_context = CompletionContext::new(
            progress.execution_epoch(),
            progress.scope_for(harvest).expect("harvest scope").clone(),
        );
        let coordinator_evidence = evidence_for(&progress, harvest, 9);
        let coordinator_observed = CancellationCoordinatorClosureObservationRecord::new(
            coordinator_manifest.clone(),
            progress.execution_epoch(),
            1,
            coordinator_manifest
                .keys()
                .iter()
                .map(|key| {
                    let outcome = if *key == harvest {
                        CancellationCoordinatorClosureOutcome::Observed {
                            evidence: coordinator_evidence.clone(),
                            context: coordinator_context.clone(),
                        }
                    } else {
                        CancellationCoordinatorClosureOutcome::Pending
                    };
                    CancellationCoordinatorClosureObservationRow::new(*key, outcome)
                })
                .collect(),
        )
        .expect("coordinator observation");
        progress
            .accept_after_effect_reconciliation(after_effect_authority(
                &progress,
                recovery.clone(),
                source_pending,
                coordinator_observed.clone(),
                progress.recovery_generation(),
            ))
            .expect("accept coordinator receipt");

        let before_lower_reacquisition = progress.clone();
        assert_eq!(
            progress.reacquire_source(source_reacquisition_authority(
                &progress,
                source_affect.participant_id(),
                8,
                2,
                progress.execution_epoch(),
            )),
            Err(WorkProgressError::SourceReacquisitionRevisionRegressed {
                observed: 8,
                minimum: 9,
            })
        );
        assert_eq!(progress, before_lower_reacquisition);
        progress
            .reacquire_source(source_reacquisition_authority(
                &progress,
                source_affect.participant_id(),
                9,
                2,
                progress.execution_epoch(),
            ))
            .expect("reacquire at coordinator receipt floor");

        let refreshed_dispatch = CancellationDispatchOutcomeRecord::new(
            dispatch_manifest.clone(),
            progress.execution_epoch(),
            2,
            dispatch_rows_with_never_entered(
                &dispatch_manifest,
                &[(
                    source_affect,
                    source_affect_evidence.clone(),
                    source_affect_context.clone(),
                )],
            ),
        )
        .expect("refreshed dispatch");
        let refreshed_recovery = progress
            .cancellation_effect_recovery_inventory(&refreshed_dispatch)
            .expect("refreshed recovery");
        let refreshed_source_manifest = progress
            .cancellation_source_closure_manifest(&refreshed_recovery, &refreshed_dispatch)
            .expect("refreshed source manifest");
        let refreshed_source = CancellationSourceClosureObservationRecord::new(
            refreshed_source_manifest.clone(),
            progress.execution_epoch(),
            2,
            source_closure_rows(&refreshed_source_manifest),
        )
        .expect("refreshed source");
        let refreshed_coordinator_manifest = progress
            .cancellation_coordinator_closure_manifest(
                &refreshed_source,
                &refreshed_recovery,
                &refreshed_dispatch,
            )
            .expect("refreshed coordinator manifest");
        let refreshed_coordinator = CancellationCoordinatorClosureObservationRecord::new(
            refreshed_coordinator_manifest.clone(),
            progress.execution_epoch(),
            2,
            refreshed_coordinator_manifest
                .keys()
                .iter()
                .map(|key| {
                    let outcome = if *key == harvest {
                        CancellationCoordinatorClosureOutcome::Observed {
                            evidence: coordinator_evidence.clone(),
                            context: coordinator_context.clone(),
                        }
                    } else {
                        CancellationCoordinatorClosureOutcome::Pending
                    };
                    CancellationCoordinatorClosureObservationRow::new(*key, outcome)
                })
                .collect(),
        )
        .expect("refreshed coordinator");
        progress
            .accept_after_effect_reconciliation(after_effect_authority(
                &progress,
                refreshed_recovery,
                refreshed_source,
                refreshed_coordinator,
                progress.recovery_generation(),
            ))
            .expect("replay historical coordinator receipt after reacquisition");
        assert_eq!(progress.execution_epoch(), 1);
        assert_eq!(progress.recovery_generation(), 2);
        let retained = progress
            .after_effect_reconciliation()
            .expect("refreshed accepted state")
            .coordinator_closure_observation()
            .rows()
            .iter()
            .find(|row| row.key() == harvest)
            .expect("retained harvest receipt");
        assert_eq!(
            retained.outcome(),
            &CancellationCoordinatorClosureOutcome::Observed {
                evidence: coordinator_evidence,
                context: coordinator_context,
            }
        );
    }

    #[test]
    fn after_effect_receipt_refinement_reuses_identical_dispatch_and_rejects_drop() {
        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let mut progress =
            claimed_progress(participants, history_folder_args(source, source_id), 1402);
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::Prepare,
            WorkUnitRole::EnsureContainedParentDirectories,
            WorkUnitRole::QualifiedRelocation,
            WorkUnitRole::OriginAbsence,
            WorkUnitRole::SourceAffect,
        ] {
            apply_step(&mut progress, 0, 0, role);
        }
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::Prepare,
            WorkUnitRole::EnsureContainedParentDirectories,
        ] {
            apply_step(&mut progress, 0, 1, role);
        }
        progress
            .request_cancellation(CancellationRequestObservation::new(1402).expect("cancel"))
            .expect("request cancellation");
        let dispatch_manifest = progress
            .cancellation_dispatch_manifest()
            .expect("dispatch manifest");
        let late_key = key_for_step(&progress, 0, 1, WorkUnitRole::QualifiedRelocation);
        let late_observation = (
            late_key,
            evidence_for(&progress, late_key, 7),
            CompletionContext::new(
                progress.execution_epoch(),
                progress.scope_for(late_key).expect("late scope").clone(),
            ),
        );
        let dispatch = CancellationDispatchOutcomeRecord::new(
            dispatch_manifest.clone(),
            progress.execution_epoch(),
            1,
            dispatch_rows_with_never_entered(&dispatch_manifest, &[late_observation]),
        )
        .expect("determinate late dispatch");
        let recovery = progress
            .cancellation_effect_recovery_inventory(&dispatch)
            .expect("recovery inventory");
        let source_manifest = progress
            .cancellation_source_closure_manifest(&recovery, &dispatch)
            .expect("source manifest");
        assert_eq!(source_manifest.keys().len(), 1);
        let source_pending = CancellationSourceClosureObservationRecord::new(
            source_manifest.clone(),
            progress.execution_epoch(),
            1,
            source_closure_rows(&source_manifest),
        )
        .expect("pending source record");
        let coordinator_manifest = progress
            .cancellation_coordinator_closure_manifest(&source_pending, &recovery, &dispatch)
            .expect("coordinator manifest");
        let coordinator_pending =
            coordinator_closure_record(coordinator_manifest, progress.execution_epoch(), 1);
        progress
            .accept_after_effect_reconciliation(after_effect_authority(
                &progress,
                recovery.clone(),
                source_pending.clone(),
                coordinator_pending.clone(),
                progress.recovery_generation(),
            ))
            .expect("accept initial state");

        let source_key = source_manifest.keys()[0];
        let source_observed = CancellationSourceClosureObservationRecord::new(
            source_manifest.clone(),
            progress.execution_epoch(),
            2,
            vec![CancellationSourceClosureObservationRow::new(
                source_key,
                CancellationSourceClosureOutcome::Observed {
                    evidence: evidence_for(&progress, source_key, 8),
                    context: CompletionContext::new(
                        0,
                        progress
                            .scope_for(source_key)
                            .expect("source scope")
                            .clone(),
                    ),
                },
            )],
        )
        .expect("observed source record");
        let coordinator_manifest = progress
            .cancellation_coordinator_closure_manifest(&source_observed, &recovery, &dispatch)
            .expect("refined coordinator manifest");
        let coordinator_pending =
            coordinator_closure_record(coordinator_manifest, progress.execution_epoch(), 2);
        progress
            .accept_after_effect_reconciliation(after_effect_authority(
                &progress,
                recovery.clone(),
                source_observed.clone(),
                coordinator_pending.clone(),
                progress.recovery_generation(),
            ))
            .expect("refine source receipt without dispatch rewrite");
        assert_eq!(progress.recovery_generation(), 2);
        assert_eq!(
            progress
                .after_effect_reconciliation()
                .expect("accepted refinement")
                .recovery_inventory()
                .dispatch_outcome()
                .recovery_generation(),
            1
        );

        let coordinator_key = coordinator_pending.manifest().keys()[0];
        let coordinator_observed = CancellationCoordinatorClosureObservationRecord::new(
            coordinator_pending.manifest().clone(),
            progress.execution_epoch(),
            3,
            coordinator_pending
                .manifest()
                .keys()
                .iter()
                .map(|key| {
                    let outcome = if *key == coordinator_key {
                        CancellationCoordinatorClosureOutcome::Observed {
                            evidence: evidence_for(&progress, *key, 8),
                            context: CompletionContext::new(
                                progress.execution_epoch(),
                                progress.scope_for(*key).expect("coordinator scope").clone(),
                            ),
                        }
                    } else {
                        CancellationCoordinatorClosureOutcome::Pending
                    };
                    CancellationCoordinatorClosureObservationRow::new(*key, outcome)
                })
                .collect(),
        )
        .expect("observed coordinator record");
        progress
            .accept_after_effect_reconciliation(after_effect_authority(
                &progress,
                recovery.clone(),
                source_observed.clone(),
                coordinator_observed.clone(),
                progress.recovery_generation(),
            ))
            .expect("refine coordinator receipt without source wrapper rewrite");
        assert_eq!(progress.recovery_generation(), 3);

        let lower_source = CancellationSourceClosureObservationRecord::new(
            source_observed.manifest().clone(),
            progress.execution_epoch(),
            1,
            source_observed.rows().to_vec(),
        )
        .expect("lower-generation source record");
        let lower_source_coordinator_manifest = progress
            .cancellation_coordinator_closure_manifest(&lower_source, &recovery, &dispatch)
            .expect("lower-source coordinator manifest");
        let lower_source_coordinator = CancellationCoordinatorClosureObservationRecord::new(
            lower_source_coordinator_manifest,
            progress.execution_epoch(),
            coordinator_observed.observation_generation(),
            coordinator_observed.rows().to_vec(),
        )
        .expect("lower-source coordinator record");
        let before_lower_source = progress.clone();
        assert_eq!(
            progress.accept_after_effect_reconciliation(after_effect_authority(
                &progress,
                recovery.clone(),
                lower_source,
                lower_source_coordinator,
                progress.recovery_generation(),
            )),
            Err(WorkProgressError::AfterEffectReconciliationReceiptReplayConflict)
        );
        assert_eq!(progress, before_lower_source);

        let lower_coordinator = CancellationCoordinatorClosureObservationRecord::new(
            coordinator_observed.manifest().clone(),
            progress.execution_epoch(),
            2,
            coordinator_observed.rows().to_vec(),
        )
        .expect("lower-generation coordinator record");
        let before_lower_coordinator = progress.clone();
        assert_eq!(
            progress.accept_after_effect_reconciliation(after_effect_authority(
                &progress,
                recovery.clone(),
                source_observed.clone(),
                lower_coordinator,
                progress.recovery_generation(),
            )),
            Err(WorkProgressError::AfterEffectReconciliationReceiptReplayConflict)
        );
        assert_eq!(progress, before_lower_coordinator);

        let same_generation_source = CancellationSourceClosureObservationRecord::new(
            source_manifest.clone(),
            progress.execution_epoch(),
            2,
            vec![CancellationSourceClosureObservationRow::new(
                source_key,
                CancellationSourceClosureOutcome::Observed {
                    evidence: evidence_for(&progress, source_key, 9),
                    context: CompletionContext::new(
                        0,
                        progress
                            .scope_for(source_key)
                            .expect("source scope")
                            .clone(),
                    ),
                },
            )],
        )
        .expect("same-generation source record");
        let same_generation_coordinator_manifest = progress
            .cancellation_coordinator_closure_manifest(
                &same_generation_source,
                &recovery,
                &dispatch,
            )
            .expect("same-generation coordinator manifest");
        let same_generation_coordinator = coordinator_closure_record(
            same_generation_coordinator_manifest,
            progress.execution_epoch(),
            2,
        );
        let before_same_generation = progress.clone();
        assert_eq!(
            progress.accept_after_effect_reconciliation(after_effect_authority(
                &progress,
                recovery.clone(),
                same_generation_source,
                same_generation_coordinator,
                progress.recovery_generation(),
            )),
            Err(WorkProgressError::AfterEffectReconciliationReceiptReplayConflict)
        );
        assert_eq!(progress, before_same_generation);

        let dropped_source = CancellationSourceClosureObservationRecord::new(
            source_manifest.clone(),
            progress.execution_epoch(),
            3,
            source_closure_rows(&source_manifest),
        )
        .expect("dropped source shape");
        let dropped_coordinator_manifest = progress
            .cancellation_coordinator_closure_manifest(&dropped_source, &recovery, &dispatch)
            .expect("dropped coordinator manifest");
        let dropped_coordinator =
            coordinator_closure_record(dropped_coordinator_manifest, progress.execution_epoch(), 3);
        let before_drop = progress.clone();
        assert_eq!(
            progress.accept_after_effect_reconciliation(after_effect_authority(
                &progress,
                recovery,
                dropped_source,
                dropped_coordinator,
                progress.recovery_generation(),
            )),
            Err(WorkProgressError::AfterEffectReconciliationReceiptReplayConflict)
        );
        assert_eq!(progress, before_drop);
    }

    #[test]
    fn cancellation_cleanup_accepts_each_nonterminal_route_and_replays_without_mutation() {
        let mut before_publish = copy_progress_with_admission_claims();
        before_publish
            .request_cancellation(CancellationRequestObservation::new(1601).expect("cancel"))
            .expect("before-publish cancellation");
        accept_empty_after_effect_reconciliation(&mut before_publish);
        let before_steps = before_publish.steps().to_vec();
        let before_inventory = before_publish.cancellation_inventory().cloned();
        let before_claims = before_publish.admission_claims().cloned();
        let before_phase = before_publish.phase();
        let before_disposition = before_publish.disposition();
        let (resource, namespace) = accept_pending_cleanup_observations(&mut before_publish);
        assert_eq!(before_publish.recovery_generation(), 2);
        assert_eq!(before_publish.phase(), before_phase);
        assert_eq!(before_publish.disposition(), before_disposition);
        assert_eq!(before_publish.steps(), before_steps.as_slice());
        assert_eq!(
            before_publish.cancellation_inventory(),
            before_inventory.as_ref()
        );
        assert_eq!(before_publish.admission_claims(), before_claims.as_ref());
        assert_eq!(
            before_publish
                .cancellation_cleanup_observations()
                .expect("accepted cleanup")
                .resource_cleanup_observation(),
            &resource
        );
        let before_replay = before_publish.clone();
        before_publish
            .accept_cancellation_cleanup_observations(cleanup_observation_authority(
                &before_publish,
                resource,
                namespace,
                before_publish.recovery_generation(),
            ))
            .expect("exact cleanup replay");
        assert_eq!(before_publish, before_replay);

        let mut after_publish = copy_progress_with_admission_claims();
        publish_prefix(&mut after_publish);
        after_publish
            .request_cancellation(CancellationRequestObservation::new(1602).expect("cancel"))
            .expect("after-publish cancellation");
        accept_empty_after_effect_reconciliation(&mut after_publish);
        accept_pending_cleanup_observations(&mut after_publish);
        assert_eq!(
            after_publish.disposition(),
            FullProgressDisposition::CancelRequestedAfterPublish
        );

        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let mut after_effect = claimed_progress(
            participants,
            retained_resolution_args(source, source_id, false),
            1603,
        );
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::Prepare,
            WorkUnitRole::SelectedRetainedObjectDisposition,
        ] {
            apply_role(&mut after_effect, 0, role);
        }
        after_effect
            .request_cancellation(CancellationRequestObservation::new(1603).expect("cancel"))
            .expect("after-observed-effect cancellation");
        accept_empty_after_effect_reconciliation(&mut after_effect);
        accept_pending_cleanup_observations(&mut after_effect);
        assert_eq!(
            after_effect.disposition(),
            FullProgressDisposition::CancelRequestedAfterObservedEffect
        );
    }

    #[test]
    fn cancellation_cleanup_rejects_audit_terminal_and_stale_authority_atomically() {
        let mut progress = copy_progress_with_admission_claims();
        publish_prefix(&mut progress);
        progress
            .request_cancellation(CancellationRequestObservation::new(1604).expect("cancel"))
            .expect("cancellation");
        accept_empty_after_effect_reconciliation(&mut progress);
        let (resource, namespace) = pending_cleanup_observation_records(&mut progress, 1);
        let authority = cleanup_observation_authority(
            &progress,
            resource.clone(),
            namespace.clone(),
            progress.recovery_generation(),
        );

        let mut wrong_lineage = authority.clone();
        wrong_lineage.after_effect_reconciliation.execution_epoch = 99;
        let before_lineage = progress.clone();
        assert_eq!(
            progress.accept_cancellation_cleanup_observations(wrong_lineage),
            Err(WorkProgressError::CancellationCleanupAfterEffectMismatch)
        );
        assert_eq!(progress, before_lineage);

        progress
            .require_audit(ProgressAuditObservation::new(
                ProgressAuditReason::LateCollision,
            ))
            .expect("audit hold");
        let before_audit = progress.clone();
        assert_eq!(
            progress.accept_cancellation_cleanup_observations(authority.clone()),
            Err(WorkProgressError::CancellationCleanupAuditActive)
        );
        assert_eq!(progress, before_audit);

        progress.active_audit = None;
        progress.disposition = FullProgressDisposition::CancelRequestedAfterPublish;
        let mut stale_epoch = authority.clone();
        stale_epoch.execution_epoch = 1;
        let before_epoch = progress.clone();
        assert_eq!(
            progress.accept_cancellation_cleanup_observations(stale_epoch),
            Err(WorkProgressError::CancellationCleanupEpochMismatch {
                expected: 0,
                actual: 1,
            })
        );
        assert_eq!(progress, before_epoch);

        let mut terminal = progress.clone();
        terminal.disposition = FullProgressDisposition::Succeeded;
        terminal.phase = OperationPhase::Terminal;
        assert_eq!(
            terminal.accept_cancellation_cleanup_observations(authority),
            Err(WorkProgressError::TerminalTransitionForbidden)
        );
    }

    #[test]
    fn cancellation_cleanup_transitions_require_generations_and_monotonic_owner_revisions() {
        let mut progress = copy_progress_with_admission_claims();
        progress
            .request_cancellation(CancellationRequestObservation::new(1605).expect("cancel"))
            .expect("cancellation");
        accept_empty_after_effect_reconciliation(&mut progress);
        let (pending_resource, pending_namespace) =
            pending_cleanup_observation_records(&mut progress, 1);
        progress
            .accept_cancellation_cleanup_observations(cleanup_observation_authority(
                &progress,
                pending_resource.clone(),
                pending_namespace.clone(),
                progress.recovery_generation(),
            ))
            .expect("pending cleanup");

        let resource_key = pending_resource.manifest().keys()[0];
        let resource_context = CompletionContext::new(
            progress.execution_epoch(),
            progress
                .scope_for(resource_key)
                .expect("resource scope")
                .clone(),
        );
        let observed_resource = CancellationResourceCleanupObservationRecord::new(
            pending_resource.manifest().clone(),
            progress.execution_epoch(),
            2,
            pending_resource
                .manifest()
                .keys()
                .iter()
                .map(|key| {
                    let outcome = if *key == resource_key {
                        CancellationResourceCleanupOutcome::Preserved {
                            owner_revision: 1,
                            context: resource_context.clone(),
                        }
                    } else {
                        CancellationResourceCleanupOutcome::Pending
                    };
                    CancellationResourceCleanupObservationRow::new(*key, outcome)
                })
                .collect(),
        )
        .expect("observed resource cleanup");
        let observed_namespace = CancellationNamespaceCleanupObservationRecord::new(
            pending_namespace.manifest().clone(),
            progress.execution_epoch(),
            2,
            pending_namespace.rows().to_vec(),
        )
        .expect("pending namespace refinement");
        progress
            .accept_cancellation_cleanup_observations(cleanup_observation_authority(
                &progress,
                observed_resource.clone(),
                observed_namespace.clone(),
                progress.recovery_generation(),
            ))
            .expect("observe resource cleanup");

        let dropped_resource = CancellationResourceCleanupObservationRecord::new(
            observed_resource.manifest().clone(),
            progress.execution_epoch(),
            3,
            pending_resource.rows().to_vec(),
        )
        .expect("dropped resource shape");
        let before_drop = progress.clone();
        assert_eq!(
            progress.accept_cancellation_cleanup_observations(cleanup_observation_authority(
                &progress,
                dropped_resource,
                observed_namespace.clone(),
                progress.recovery_generation(),
            )),
            Err(
                WorkProgressError::CancellationCleanupObservationReplayConflict {
                    key: resource_key,
                }
            )
        );
        assert_eq!(progress, before_drop);

        let stale_revision = CancellationResourceCleanupObservationRecord::new(
            observed_resource.manifest().clone(),
            progress.execution_epoch(),
            3,
            observed_resource
                .manifest()
                .keys()
                .iter()
                .map(|key| {
                    let outcome = if *key == resource_key {
                        CancellationResourceCleanupOutcome::Absent {
                            owner_revision: 1,
                            context: resource_context.clone(),
                        }
                    } else {
                        CancellationResourceCleanupOutcome::Pending
                    };
                    CancellationResourceCleanupObservationRow::new(*key, outcome)
                })
                .collect(),
        )
        .expect("stale revision shape");
        let before_revision = progress.clone();
        assert_eq!(
            progress.accept_cancellation_cleanup_observations(cleanup_observation_authority(
                &progress,
                stale_revision,
                observed_namespace.clone(),
                progress.recovery_generation(),
            )),
            Err(
                WorkProgressError::CancellationCleanupOwnerRevisionRegression {
                    key: resource_key,
                    previous: 1,
                    actual: 1,
                }
            )
        );
        assert_eq!(progress, before_revision);

        let lower_generation = CancellationResourceCleanupObservationRecord::new(
            observed_resource.manifest().clone(),
            progress.execution_epoch(),
            2,
            observed_resource
                .manifest()
                .keys()
                .iter()
                .map(|key| {
                    let outcome = if *key == resource_key {
                        CancellationResourceCleanupOutcome::Absent {
                            owner_revision: 2,
                            context: resource_context.clone(),
                        }
                    } else {
                        CancellationResourceCleanupOutcome::Pending
                    };
                    CancellationResourceCleanupObservationRow::new(*key, outcome)
                })
                .collect(),
        )
        .expect("lower generation shape");
        let before_generation = progress.clone();
        assert_eq!(
            progress.accept_cancellation_cleanup_observations(cleanup_observation_authority(
                &progress,
                lower_generation,
                observed_namespace.clone(),
                progress.recovery_generation(),
            )),
            Err(
                WorkProgressError::CancellationCleanupObservationGenerationRegression {
                    previous: 2,
                    actual: 2,
                }
            )
        );
        assert_eq!(progress, before_generation);

        let fresh_resource = CancellationResourceCleanupObservationRecord::new(
            observed_resource.manifest().clone(),
            progress.execution_epoch(),
            3,
            observed_resource
                .manifest()
                .keys()
                .iter()
                .map(|key| {
                    let outcome = if *key == resource_key {
                        CancellationResourceCleanupOutcome::Absent {
                            owner_revision: 2,
                            context: resource_context.clone(),
                        }
                    } else {
                        CancellationResourceCleanupOutcome::Pending
                    };
                    CancellationResourceCleanupObservationRow::new(*key, outcome)
                })
                .collect(),
        )
        .expect("fresh revision shape");
        progress
            .accept_cancellation_cleanup_observations(cleanup_observation_authority(
                &progress,
                fresh_resource,
                observed_namespace,
                progress.recovery_generation(),
            ))
            .expect("fresh owner revision");
        assert_eq!(progress.recovery_generation(), 4);
    }

    #[test]
    fn cancellation_cleanup_reuses_historical_rows_only_after_current_header_refresh() {
        let mut first = copy_progress_with_admission_claims();
        first
            .request_cancellation(CancellationRequestObservation::new(1606).expect("cancel"))
            .expect("cancellation");
        accept_empty_after_effect_reconciliation(&mut first);
        let source_key = key_for(&first, 0, WorkUnitRole::SourceRead);
        let resource_key = key_for(&first, 0, WorkUnitRole::Stage);
        let resource_context = CompletionContext::new(
            first.execution_epoch(),
            first
                .scope_for(resource_key)
                .expect("resource scope")
                .clone(),
        );
        let namespace_key = key_for(&first, 0, WorkUnitRole::Publish);
        let namespace_context = CompletionContext::new(
            first.execution_epoch(),
            first
                .scope_for(namespace_key)
                .expect("namespace scope")
                .clone(),
        );
        first
            .reacquire_source(source_reacquisition_authority(
                &first,
                source_key.participant_id(),
                7,
                2,
                first.execution_epoch(),
            ))
            .expect("reacquire before first cleanup");
        refresh_after_effect_headers_after_reacquisition(&mut first);
        let (resource, namespace) = pending_cleanup_observation_records(&mut first, 1);
        let resource = CancellationResourceCleanupObservationRecord::new(
            resource.manifest().clone(),
            first.execution_epoch(),
            1,
            resource
                .manifest()
                .keys()
                .iter()
                .map(|key| {
                    let outcome = if *key == resource_key {
                        CancellationResourceCleanupOutcome::Preserved {
                            owner_revision: 1,
                            context: resource_context.clone(),
                        }
                    } else {
                        CancellationResourceCleanupOutcome::Pending
                    };
                    CancellationResourceCleanupObservationRow::new(*key, outcome)
                })
                .collect(),
        )
        .expect("first historical resource record");
        let namespace = CancellationNamespaceCleanupObservationRecord::new(
            namespace.manifest().clone(),
            first.execution_epoch(),
            1,
            namespace
                .manifest()
                .keys()
                .iter()
                .map(|key| {
                    let outcome = if *key == namespace_key {
                        CancellationNamespaceCleanupOutcome::Preserved {
                            owner_revision: 1,
                            context: namespace_context.clone(),
                        }
                    } else {
                        CancellationNamespaceCleanupOutcome::Pending
                    };
                    CancellationNamespaceCleanupObservationRow::new(*key, outcome)
                })
                .collect(),
        )
        .expect("first historical namespace record");
        let before_first_accept = first.clone();
        assert_eq!(
            first.accept_cancellation_cleanup_observations(cleanup_observation_authority(
                &first,
                resource,
                namespace,
                first.recovery_generation(),
            )),
            Err(
                WorkProgressError::CancellationCleanupObservationEpochMismatch {
                    key: resource_key,
                    expected: 1,
                    actual: 0,
                }
            )
        );
        assert_eq!(first, before_first_accept);

        let (resource, namespace) = pending_cleanup_observation_records(&mut first, 1);
        let historical_namespace = CancellationNamespaceCleanupObservationRecord::new(
            namespace.manifest().clone(),
            first.execution_epoch(),
            1,
            namespace
                .manifest()
                .keys()
                .iter()
                .map(|key| {
                    let outcome = if *key == namespace_key {
                        CancellationNamespaceCleanupOutcome::Preserved {
                            owner_revision: 1,
                            context: namespace_context.clone(),
                        }
                    } else {
                        CancellationNamespaceCleanupOutcome::Pending
                    };
                    CancellationNamespaceCleanupObservationRow::new(*key, outcome)
                })
                .collect(),
        )
        .expect("second historical namespace record");
        let before_namespace_accept = first.clone();
        assert_eq!(
            first.accept_cancellation_cleanup_observations(cleanup_observation_authority(
                &first,
                resource,
                historical_namespace,
                first.recovery_generation(),
            )),
            Err(
                WorkProgressError::CancellationCleanupObservationEpochMismatch {
                    key: namespace_key,
                    expected: 1,
                    actual: 0,
                }
            )
        );
        assert_eq!(first, before_namespace_accept);

        let (resource, namespace) = pending_cleanup_observation_records(&mut first, 1);
        first
            .accept_cancellation_cleanup_observations(cleanup_observation_authority(
                &first,
                resource,
                namespace,
                first.recovery_generation(),
            ))
            .expect("first pending cleanup after refresh");
        assert_eq!(first.execution_epoch(), 1);

        let mut reuse = copy_progress_with_admission_claims();
        reuse
            .request_cancellation(CancellationRequestObservation::new(1607).expect("cancel"))
            .expect("cancellation");
        accept_empty_after_effect_reconciliation(&mut reuse);
        let source_key = key_for(&reuse, 0, WorkUnitRole::SourceRead);
        let resource_key = key_for(&reuse, 0, WorkUnitRole::Stage);
        let historical_context = CompletionContext::new(
            reuse.execution_epoch(),
            reuse
                .scope_for(resource_key)
                .expect("resource scope")
                .clone(),
        );
        let (resource, namespace) = pending_cleanup_observation_records(&mut reuse, 1);
        let resource = CancellationResourceCleanupObservationRecord::new(
            resource.manifest().clone(),
            reuse.execution_epoch(),
            1,
            resource
                .manifest()
                .keys()
                .iter()
                .map(|key| {
                    let outcome = if *key == resource_key {
                        CancellationResourceCleanupOutcome::Preserved {
                            owner_revision: 1,
                            context: historical_context.clone(),
                        }
                    } else {
                        CancellationResourceCleanupOutcome::Pending
                    };
                    CancellationResourceCleanupObservationRow::new(*key, outcome)
                })
                .collect(),
        )
        .expect("initial resource observation");
        reuse
            .accept_cancellation_cleanup_observations(cleanup_observation_authority(
                &reuse,
                resource.clone(),
                namespace.clone(),
                reuse.recovery_generation(),
            ))
            .expect("initial current resource observation");
        reuse
            .reacquire_source(source_reacquisition_authority(
                &reuse,
                source_key.participant_id(),
                7,
                2,
                reuse.execution_epoch(),
            ))
            .expect("reacquire with cleanup history");
        refresh_after_effect_headers_after_reacquisition(&mut reuse);
        let (refreshed_resource_manifest, refreshed_namespace_manifest) = {
            let after_effect = reuse
                .after_effect_reconciliation
                .as_ref()
                .expect("refreshed after-effect");
            let recovery = after_effect.recovery_inventory();
            let dispatch = recovery.dispatch_outcome();
            let cleanup = reuse
                .cancellation_cleanup_manifest(recovery, dispatch)
                .expect("refreshed cleanup");
            (
                reuse
                    .cancellation_resource_cleanup_manifest(&cleanup, recovery, dispatch)
                    .expect("refreshed resource"),
                reuse
                    .cancellation_namespace_cleanup_manifest(&cleanup, recovery, dispatch)
                    .expect("refreshed namespace"),
            )
        };
        let refreshed_resource = CancellationResourceCleanupObservationRecord::new(
            refreshed_resource_manifest.clone(),
            reuse.execution_epoch(),
            2,
            refreshed_resource_manifest
                .keys()
                .iter()
                .map(|key| {
                    let outcome = if *key == resource_key {
                        CancellationResourceCleanupOutcome::Preserved {
                            owner_revision: 1,
                            context: historical_context.clone(),
                        }
                    } else {
                        CancellationResourceCleanupOutcome::Pending
                    };
                    CancellationResourceCleanupObservationRow::new(*key, outcome)
                })
                .collect(),
        )
        .expect("refreshed historical resource");
        let refreshed_namespace = CancellationNamespaceCleanupObservationRecord::new(
            refreshed_namespace_manifest.clone(),
            reuse.execution_epoch(),
            2,
            pending_namespace_cleanup_rows(&refreshed_namespace_manifest),
        )
        .expect("refreshed namespace");
        let before_reuse = reuse.clone();
        reuse
            .accept_cancellation_cleanup_observations(cleanup_observation_authority(
                &reuse,
                refreshed_resource,
                refreshed_namespace,
                reuse.recovery_generation(),
            ))
            .expect("reuse historical cleanup rows after header refresh");
        assert_eq!(reuse.execution_epoch(), 1);
        assert_eq!(reuse.steps(), before_reuse.steps());
        assert_eq!(
            reuse.cancellation_inventory(),
            before_reuse.cancellation_inventory()
        );
        assert_eq!(reuse.recovery_generation(), 4);
    }

    #[test]
    fn cancellation_cleanup_requires_refresh_when_dispatch_effects_change() {
        let mut progress = fenced_harvest_progress_with_admission_claims();
        progress
            .request_cancellation(CancellationRequestObservation::new(1608).expect("cancel"))
            .expect("cancellation");
        let dispatch_manifest = progress
            .cancellation_dispatch_manifest()
            .expect("dispatch manifest");
        let uncertain_dispatch = CancellationDispatchOutcomeRecord::new(
            dispatch_manifest.clone(),
            progress.execution_epoch(),
            1,
            dispatch_rows_with_observed(&dispatch_manifest, &[]),
        )
        .expect("uncertain dispatch");
        let (recovery, source, coordinator) =
            empty_after_effect_inputs(&progress, uncertain_dispatch);
        progress
            .accept_after_effect_reconciliation(after_effect_authority(
                &progress,
                recovery,
                source,
                coordinator,
                progress.recovery_generation(),
            ))
            .expect("accept uncertain after-effect state");

        let (resource, namespace) = pending_cleanup_observation_records(&mut progress, 1);
        let resource_key = resource.manifest().keys()[0];
        let resource_context = CompletionContext::new(
            progress.execution_epoch(),
            progress
                .scope_for(resource_key)
                .expect("resource scope")
                .clone(),
        );
        let observed_resource = CancellationResourceCleanupObservationRecord::new(
            resource.manifest().clone(),
            progress.execution_epoch(),
            1,
            resource
                .manifest()
                .keys()
                .iter()
                .map(|key| {
                    let outcome = if *key == resource_key {
                        CancellationResourceCleanupOutcome::Preserved {
                            owner_revision: 1,
                            context: resource_context.clone(),
                        }
                    } else {
                        CancellationResourceCleanupOutcome::Pending
                    };
                    CancellationResourceCleanupObservationRow::new(*key, outcome)
                })
                .collect(),
        )
        .expect("observed resource");
        progress
            .accept_cancellation_cleanup_observations(cleanup_observation_authority(
                &progress,
                observed_resource.clone(),
                namespace.clone(),
                progress.recovery_generation(),
            ))
            .expect("accept initial cleanup observation");

        let source_affect = key_for(&progress, 0, WorkUnitRole::SourceAffect);
        assert!(dispatch_manifest.keys().contains(&source_affect));
        let late_evidence = evidence_for(&progress, source_affect, 7);
        let late_context = CompletionContext::new(
            progress.execution_epoch(),
            progress
                .scope_for(source_affect)
                .expect("source-affect scope")
                .clone(),
        );
        let changed_dispatch = CancellationDispatchOutcomeRecord::new(
            dispatch_manifest.clone(),
            progress.execution_epoch(),
            progress
                .recovery_generation()
                .checked_add(1)
                .expect("dispatch generation"),
            dispatch_rows_with_observed(
                &dispatch_manifest,
                &[(source_affect, late_evidence, late_context)],
            ),
        )
        .expect("changed dispatch");
        let changed_recovery = progress
            .cancellation_effect_recovery_inventory(&changed_dispatch)
            .expect("changed recovery");
        let changed_source_manifest = progress
            .cancellation_source_closure_manifest(&changed_recovery, &changed_dispatch)
            .expect("changed source manifest");
        let changed_source = CancellationSourceClosureObservationRecord::new(
            changed_source_manifest.clone(),
            progress.execution_epoch(),
            2,
            source_closure_rows(&changed_source_manifest),
        )
        .expect("changed source");
        let changed_coordinator_manifest = progress
            .cancellation_coordinator_closure_manifest(
                &changed_source,
                &changed_recovery,
                &changed_dispatch,
            )
            .expect("changed coordinator manifest");
        let changed_coordinator =
            coordinator_closure_record(changed_coordinator_manifest, progress.execution_epoch(), 2);
        progress
            .accept_after_effect_reconciliation(after_effect_authority(
                &progress,
                changed_recovery.clone(),
                changed_source,
                changed_coordinator,
                progress.recovery_generation(),
            ))
            .expect("accept changed dispatch effect");

        let changed_cleanup = progress
            .cancellation_cleanup_manifest(&changed_recovery, &changed_dispatch)
            .expect("changed cleanup manifest");
        let changed_resource_manifest = progress
            .cancellation_resource_cleanup_manifest(
                &changed_cleanup,
                &changed_recovery,
                &changed_dispatch,
            )
            .expect("changed resource manifest");
        let changed_namespace_manifest = progress
            .cancellation_namespace_cleanup_manifest(
                &changed_cleanup,
                &changed_recovery,
                &changed_dispatch,
            )
            .expect("changed namespace manifest");
        let stale_resource = CancellationResourceCleanupObservationRecord::new(
            changed_resource_manifest.clone(),
            progress.execution_epoch(),
            2,
            changed_resource_manifest
                .keys()
                .iter()
                .map(|key| {
                    let outcome = if *key == resource_key {
                        CancellationResourceCleanupOutcome::Preserved {
                            owner_revision: 1,
                            context: resource_context.clone(),
                        }
                    } else {
                        CancellationResourceCleanupOutcome::Pending
                    };
                    CancellationResourceCleanupObservationRow::new(*key, outcome)
                })
                .collect(),
        )
        .expect("stale changed-dispatch resource");
        let changed_namespace = CancellationNamespaceCleanupObservationRecord::new(
            changed_namespace_manifest.clone(),
            progress.execution_epoch(),
            2,
            pending_namespace_cleanup_rows(&changed_namespace_manifest),
        )
        .expect("changed namespace");
        let before_stale = progress.clone();
        assert_eq!(
            progress.accept_cancellation_cleanup_observations(cleanup_observation_authority(
                &progress,
                stale_resource,
                changed_namespace.clone(),
                progress.recovery_generation(),
            )),
            Err(
                WorkProgressError::CancellationCleanupOwnerRevisionRegression {
                    key: resource_key,
                    previous: 1,
                    actual: 1,
                }
            )
        );
        assert_eq!(progress, before_stale);

        let fresh_resource = CancellationResourceCleanupObservationRecord::new(
            changed_resource_manifest.clone(),
            progress.execution_epoch(),
            2,
            changed_resource_manifest
                .keys()
                .iter()
                .map(|key| {
                    let outcome = if *key == resource_key {
                        CancellationResourceCleanupOutcome::Preserved {
                            owner_revision: 2,
                            context: resource_context.clone(),
                        }
                    } else {
                        CancellationResourceCleanupOutcome::Pending
                    };
                    CancellationResourceCleanupObservationRow::new(*key, outcome)
                })
                .collect(),
        )
        .expect("fresh changed-dispatch resource");
        progress
            .accept_cancellation_cleanup_observations(cleanup_observation_authority(
                &progress,
                fresh_resource,
                changed_namespace,
                progress.recovery_generation(),
            ))
            .expect("accept refreshed cleanup observation");
        assert_eq!(progress.recovery_generation(), 4);

        let accepted_after_effect = progress
            .after_effect_reconciliation
            .as_ref()
            .expect("accepted changed after-effect")
            .clone();
        let refined_source = CancellationSourceClosureObservationRecord::new(
            accepted_after_effect
                .source_closure_observation()
                .manifest()
                .clone(),
            progress.execution_epoch(),
            accepted_after_effect
                .source_closure_observation()
                .observation_generation()
                .checked_add(1)
                .expect("source refinement generation"),
            accepted_after_effect
                .source_closure_observation()
                .rows()
                .to_vec(),
        )
        .expect("refined source-only observation");
        let refined_coordinator_manifest = progress
            .cancellation_coordinator_closure_manifest(
                &refined_source,
                accepted_after_effect.recovery_inventory(),
                accepted_after_effect
                    .recovery_inventory()
                    .dispatch_outcome(),
            )
            .expect("refined coordinator manifest");
        let refined_coordinator = CancellationCoordinatorClosureObservationRecord::new(
            refined_coordinator_manifest,
            progress.execution_epoch(),
            accepted_after_effect
                .coordinator_closure_observation()
                .observation_generation()
                .checked_add(1)
                .expect("coordinator refinement generation"),
            accepted_after_effect
                .coordinator_closure_observation()
                .rows()
                .to_vec(),
        )
        .expect("refined coordinator-only observation");
        progress
            .accept_after_effect_reconciliation(after_effect_authority(
                &progress,
                accepted_after_effect.recovery_inventory().clone(),
                refined_source,
                refined_coordinator,
                progress.recovery_generation(),
            ))
            .expect("accept source/coordinator-only refinement");
        assert_eq!(progress.recovery_generation(), 5);
        let retained_resource = progress
            .cancellation_cleanup_observations()
            .expect("retained cleanup after-effect refinement")
            .resource_cleanup_observation()
            .clone();
        let retained_namespace = progress
            .cancellation_cleanup_observations()
            .expect("retained namespace after-effect refinement")
            .namespace_cleanup_observation()
            .clone();
        progress
            .accept_cancellation_cleanup_observations(cleanup_observation_authority(
                &progress,
                retained_resource,
                retained_namespace,
                progress.recovery_generation(),
            ))
            .expect("reuse cleanup rows after source/coordinator refinement");
        assert_eq!(progress.recovery_generation(), 6);
    }

    #[test]
    fn cancellation_cleanup_exact_replay_at_max_generation_is_stable_and_new_state_overflows() {
        let mut progress = copy_progress_with_admission_claims();
        progress
            .request_cancellation(CancellationRequestObservation::new(1609).expect("cancel"))
            .expect("cancellation");
        accept_empty_after_effect_reconciliation(&mut progress);
        let (resource, namespace) = accept_pending_cleanup_observations(&mut progress);
        progress
            .cancellation_cleanup_observations
            .as_mut()
            .expect("accepted cleanup")
            .recovery_generation = u64::MAX;
        progress.recovery_generation = u64::MAX;
        let replay =
            cleanup_observation_authority(&progress, resource.clone(), namespace.clone(), u64::MAX);
        let before_replay = progress.clone();
        progress
            .accept_cancellation_cleanup_observations(replay)
            .expect("maximum-generation cleanup replay");
        assert_eq!(progress, before_replay);

        let resource_key = resource.manifest().keys()[0];
        let context = CompletionContext::new(
            progress.execution_epoch(),
            progress
                .scope_for(resource_key)
                .expect("resource scope")
                .clone(),
        );
        let changed_resource = CancellationResourceCleanupObservationRecord::new(
            resource.manifest().clone(),
            progress.execution_epoch(),
            2,
            resource
                .manifest()
                .keys()
                .iter()
                .map(|key| {
                    let outcome = if *key == resource_key {
                        CancellationResourceCleanupOutcome::Preserved {
                            owner_revision: 1,
                            context: context.clone(),
                        }
                    } else {
                        CancellationResourceCleanupOutcome::Pending
                    };
                    CancellationResourceCleanupObservationRow::new(*key, outcome)
                })
                .collect(),
        )
        .expect("changed maximum-generation resource");
        let before_overflow = progress.clone();
        assert_eq!(
            progress.accept_cancellation_cleanup_observations(cleanup_observation_authority(
                &progress,
                changed_resource,
                namespace,
                u64::MAX,
            )),
            Err(WorkProgressError::CancellationCleanupGenerationOverflow)
        );
        assert_eq!(progress, before_overflow);
    }

    #[test]
    fn live_dispatch_owner_consumes_only_valid_normal_completion() {
        let mut owner =
            dispatch_owner::LiveDispatchOwner::new_for_test(copy_progress_with_admission_claims())
                .expect("live owner");
        let source = key_for(owner.progress(), 0, WorkUnitRole::SourceRead);
        let reserved = owner.issue(source).expect("reserve source");
        let entered = owner.enter(reserved).expect("enter source");
        let before = owner.progress().clone();

        let failure = owner
            .complete(
                entered,
                StoredStepEvidence::Filesystem {
                    receipt: FilesystemReceiptSummary::new(FilesystemReceiptKind::Stage, 1),
                },
            )
            .expect_err("wrong evidence must be retryable");
        assert_eq!(
            failure.error(),
            &dispatch_owner::DispatchOwnerError::Progress(
                WorkProgressError::RoleEvidenceMismatch {
                    role: WorkUnitRole::SourceRead,
                },
            )
        );
        let (error, entered) = failure.into_parts();
        assert_eq!(
            error,
            dispatch_owner::DispatchOwnerError::Progress(WorkProgressError::RoleEvidenceMismatch {
                role: WorkUnitRole::SourceRead,
            },)
        );
        assert_eq!(owner.progress(), &before);
        assert_eq!(
            owner.ledger_state(source),
            Some(dispatch_owner::DispatchLedgerState::Entered)
        );

        assert_eq!(
            owner
                .complete(entered, evidence_for(owner.progress(), source, 7))
                .expect("valid source completion"),
            ProgressApplyOutcome::Applied
        );
        assert_eq!(
            owner
                .progress()
                .step_for(source)
                .expect("source step")
                .status(),
            ProgressStepStatus::Completed
        );
        assert_eq!(
            owner.ledger_state(source),
            Some(dispatch_owner::DispatchLedgerState::CompletedNormally)
        );
    }

    #[test]
    fn live_dispatch_owner_rejects_resumed_progress_as_a_fresh_fixture() {
        let mut progress = copy_progress_with_admission_claims();
        progress
            .request_cancellation(CancellationRequestObservation::new(17005).expect("cancel"))
            .expect("cancel resumed seed");
        assert!(matches!(
            dispatch_owner::LiveDispatchOwner::new_for_test(progress),
            Err(dispatch_owner::DispatchOwnerError::NonFreshProgressSeed)
        ));
    }

    #[test]
    fn live_dispatch_owner_cancellation_closes_entry_and_seals_after_return() {
        let mut owner =
            dispatch_owner::LiveDispatchOwner::new_for_test(copy_progress_with_admission_claims())
                .expect("live owner");
        let source = key_for(owner.progress(), 0, WorkUnitRole::SourceRead);
        let reserved = owner.issue(source).expect("reserve source");
        let request = CancellationRequestObservation::new(1701).expect("cancel request");
        owner
            .request_cancellation(request)
            .expect("request cancellation");
        assert_eq!(
            owner.lifecycle(),
            dispatch_owner::DispatchOwnerLifecycle::CancelDraining
        );

        let pending_non_source = owner
            .progress()
            .steps()
            .iter()
            .find(|step| {
                step.status() == ProgressStepStatus::Pending
                    && step.key().role() != WorkUnitRole::SourceRead
            })
            .map(WorkProgressStep::key)
            .expect("pending non-source key");
        assert!(matches!(
            owner.issue(pending_non_source),
            Err(dispatch_owner::DispatchOwnerError::IssuanceClosed)
        ));
        let manifest = owner
            .progress()
            .cancellation_dispatch_manifest()
            .expect("frozen dispatch manifest");
        assert!(manifest.keys().contains(&pending_non_source));

        let failure = owner
            .enter(reserved)
            .expect_err("cancellation must reject entry");
        assert_eq!(
            failure.error(),
            &dispatch_owner::DispatchOwnerError::IssuanceClosed
        );
        let (error, reserved) = failure.into_parts();
        assert_eq!(error, dispatch_owner::DispatchOwnerError::IssuanceClosed);
        assert_eq!(
            owner.ledger_state(source),
            Some(dispatch_owner::DispatchLedgerState::Reserved)
        );
        assert!(matches!(
            owner.seal(),
            Err(dispatch_owner::DispatchOwnerError::OutstandingPermits { count: 1 })
        ));

        owner
            .return_reserved(reserved)
            .expect("explicit no-entry return");
        let barrier = owner.seal().expect("sealed barrier");
        assert_eq!(
            owner.lifecycle(),
            dispatch_owner::DispatchOwnerLifecycle::Sealed
        );
        assert!(
            barrier
                .dispatch_outcome()
                .rows()
                .iter()
                .all(|row| row.key().role() != WorkUnitRole::SourceRead)
        );
        assert!(
            barrier
                .dispatch_outcome()
                .rows()
                .iter()
                .find(|row| row.key() == pending_non_source)
                .is_some_and(|row| row.outcome() == &CancellationDispatchOutcome::NeverEntered)
        );
        let sealed_record = barrier.dispatch_outcome().clone();
        assert!(barrier.validate_for_owner(&owner).is_ok());

        owner
            .request_cancellation(request)
            .expect("exact cancellation replay");
        let replay = owner.seal().expect("exact seal replay");
        assert_eq!(replay.dispatch_outcome(), &sealed_record);
        assert!(replay.validate_for_owner(&owner).is_ok());
    }

    #[test]
    fn live_dispatch_owner_preparation_completion_keeps_later_work_never_entered() {
        let mut owner =
            dispatch_owner::LiveDispatchOwner::new_for_test(copy_progress_with_admission_claims())
                .expect("live owner");
        let source = key_for(owner.progress(), 0, WorkUnitRole::SourceRead);
        let source_reserved = owner.issue(source).expect("source reservation");
        let source_entered = owner.enter(source_reserved).expect("source entry");
        owner
            .complete(source_entered, evidence_for(owner.progress(), source, 7))
            .expect("source completion");
        let prepare = key_for(owner.progress(), 0, WorkUnitRole::Prepare);
        let prepare_reserved = owner.issue(prepare).expect("prepare reservation");
        let prepare_entered = owner.enter(prepare_reserved).expect("prepare entry");
        owner
            .complete(prepare_entered, evidence_for(owner.progress(), prepare, 7))
            .expect("prepare completion");
        let stage = key_for(owner.progress(), 0, WorkUnitRole::Stage);
        let publish = key_for(owner.progress(), 0, WorkUnitRole::Publish);
        assert!(matches!(
            owner.issue(publish),
            Err(dispatch_owner::DispatchOwnerError::PrerequisiteIncomplete {
                dependent,
                ..
            }) if dependent == publish
        ));
        let before_steps = owner.progress().steps().to_vec();
        owner
            .request_cancellation(CancellationRequestObservation::new(17017).expect("cancel"))
            .expect("preparation-only cancellation");
        let manifest = owner
            .progress()
            .cancellation_dispatch_manifest()
            .expect("frozen dispatch manifest");
        assert!(
            owner
                .progress()
                .cancellation_effect_inventory()
                .expect("prepared-only effect inventory")
                .bundles()
                .is_empty()
        );
        assert!(!manifest.keys().contains(&prepare));
        assert!(manifest.keys().contains(&stage));
        assert!(manifest.keys().contains(&publish));
        let barrier = owner.seal().expect("preparation-only barrier");
        assert_eq!(owner.progress().steps(), before_steps.as_slice());
        assert!(
            barrier
                .dispatch_outcome()
                .rows()
                .iter()
                .find(|row| row.key() == stage)
                .is_some_and(|row| row.outcome() == &CancellationDispatchOutcome::NeverEntered)
        );
        assert!(
            barrier
                .dispatch_outcome()
                .rows()
                .iter()
                .find(|row| row.key() == publish)
                .is_some_and(|row| row.outcome() == &CancellationDispatchOutcome::NeverEntered)
        );
    }

    #[test]
    fn live_dispatch_owner_dropped_permits_remain_outstanding() {
        let mut reserved_owner =
            dispatch_owner::LiveDispatchOwner::new_for_test(copy_progress_with_admission_claims())
                .expect("reserved owner");
        let source = key_for(reserved_owner.progress(), 0, WorkUnitRole::SourceRead);
        let reserved = reserved_owner.issue(source).expect("source reservation");
        reserved_owner
            .request_cancellation(CancellationRequestObservation::new(17015).expect("cancel"))
            .expect("reserved cancellation");
        drop(reserved);
        assert!(matches!(
            reserved_owner.seal(),
            Err(dispatch_owner::DispatchOwnerError::OutstandingPermits { count: 1 })
        ));

        let mut entered_owner =
            dispatch_owner::LiveDispatchOwner::new_for_test(copy_progress_with_admission_claims())
                .expect("entered owner");
        let source = key_for(entered_owner.progress(), 0, WorkUnitRole::SourceRead);
        let reserved = entered_owner.issue(source).expect("source reservation");
        let entered = entered_owner.enter(reserved).expect("source entry");
        entered_owner
            .request_cancellation(CancellationRequestObservation::new(17016).expect("cancel"))
            .expect("entered cancellation");
        drop(entered);
        assert!(matches!(
            entered_owner.seal(),
            Err(dispatch_owner::DispatchOwnerError::OutstandingPermits { count: 1 })
        ));
    }

    #[test]
    fn live_dispatch_owner_retains_uncertain_and_late_outcomes_without_step_mutation() {
        let mut entered_uncertain =
            dispatch_owner::LiveDispatchOwner::new_for_test(copy_progress_with_admission_claims())
                .expect("entered-uncertain owner");
        let source = key_for(entered_uncertain.progress(), 0, WorkUnitRole::SourceRead);
        let source_reserved = entered_uncertain.issue(source).expect("source reservation");
        let source_entered = entered_uncertain
            .enter(source_reserved)
            .expect("source entry");
        entered_uncertain
            .request_cancellation(CancellationRequestObservation::new(1700).expect("cancel"))
            .expect("entered-uncertain cancellation");
        assert!(matches!(
            entered_uncertain.seal(),
            Err(dispatch_owner::DispatchOwnerError::OutstandingPermits { count: 1 })
        ));
        entered_uncertain
            .retire_entered_uncertain(source_entered)
            .expect("entered-uncertain retirement");
        assert_eq!(
            entered_uncertain.ledger_state(source),
            Some(dispatch_owner::DispatchLedgerState::ReturnedUncertain)
        );
        let entered_uncertain_barrier = entered_uncertain.seal().expect("uncertain barrier");
        assert!(
            entered_uncertain_barrier
                .dispatch_outcome()
                .rows()
                .iter()
                .all(|row| row.key().role() != WorkUnitRole::SourceRead)
        );

        let mut uncertain =
            dispatch_owner::LiveDispatchOwner::new_for_test(copy_progress_with_admission_claims())
                .expect("uncertain owner");
        let source = key_for(uncertain.progress(), 0, WorkUnitRole::SourceRead);
        let source_reserved = uncertain.issue(source).expect("source reservation");
        let source_entered = uncertain.enter(source_reserved).expect("source entry");
        uncertain
            .complete(
                source_entered,
                evidence_for(uncertain.progress(), source, 7),
            )
            .expect("source completion");
        let prepare = key_for(uncertain.progress(), 0, WorkUnitRole::Prepare);
        let prepare_reserved = uncertain.issue(prepare).expect("prepare reservation");
        uncertain
            .retire_reserved_uncertain(prepare_reserved)
            .expect("uncertain reservation retirement");
        uncertain
            .request_cancellation(CancellationRequestObservation::new(1702).expect("cancel"))
            .expect("uncertain cancellation");
        let uncertain_barrier = uncertain.seal().expect("uncertain barrier");
        assert!(
            uncertain_barrier
                .dispatch_outcome()
                .rows()
                .iter()
                .find(|row| row.key() == prepare)
                .is_some_and(|row| {
                    row.outcome() == &CancellationDispatchOutcome::EntryUncertain
                })
        );

        let mut failed_entry =
            dispatch_owner::LiveDispatchOwner::new_for_test(copy_progress_with_admission_claims())
                .expect("failed-entry owner");
        let source = key_for(failed_entry.progress(), 0, WorkUnitRole::SourceRead);
        let source_reserved = failed_entry.issue(source).expect("source reservation");
        let source_entered = failed_entry.enter(source_reserved).expect("source entry");
        failed_entry
            .complete(
                source_entered,
                evidence_for(failed_entry.progress(), source, 7),
            )
            .expect("source completion");
        let prepare = key_for(failed_entry.progress(), 0, WorkUnitRole::Prepare);
        let prepare_reserved = failed_entry.issue(prepare).expect("prepare reservation");
        let prepare_entered = failed_entry.enter(prepare_reserved).expect("prepare entry");
        let failure = failed_entry
            .complete(
                prepare_entered,
                StoredStepEvidence::Filesystem {
                    receipt: FilesystemReceiptSummary::new(FilesystemReceiptKind::Stage, 1),
                },
            )
            .expect_err("failed normal entry must remain uncertain");
        assert!(matches!(
            failure.error(),
            dispatch_owner::DispatchOwnerError::Progress(WorkProgressError::RoleEvidenceMismatch {
                role: WorkUnitRole::Prepare
            })
        ));
        let (_, prepare_entered) = failure.into_parts();
        failed_entry
            .request_cancellation(CancellationRequestObservation::new(17021).expect("cancel"))
            .expect("failed-entry cancellation");
        failed_entry
            .retire_entered_uncertain(prepare_entered)
            .expect("failed-entry uncertainty");
        let failed_entry_barrier = failed_entry.seal().expect("failed-entry barrier");
        assert!(
            failed_entry_barrier
                .dispatch_outcome()
                .rows()
                .iter()
                .find(|row| row.key() == prepare)
                .is_some_and(|row| row.outcome() == &CancellationDispatchOutcome::EntryUncertain)
        );

        let mut source_late =
            dispatch_owner::LiveDispatchOwner::new_for_test(copy_progress_with_admission_claims())
                .expect("source-late owner");
        let source = key_for(source_late.progress(), 0, WorkUnitRole::SourceRead);
        let source_reserved = source_late.issue(source).expect("source reservation");
        let source_entered = source_late.enter(source_reserved).expect("source entry");
        source_late
            .request_cancellation(CancellationRequestObservation::new(17025).expect("cancel"))
            .expect("source-late cancellation");
        source_late
            .retire_entered_completed(
                source_entered,
                evidence_for(source_late.progress(), source, 7),
            )
            .expect("late source completion");
        assert_eq!(
            source_late.ledger_state(source),
            Some(dispatch_owner::DispatchLedgerState::CompletedAfterCancellation)
        );
        let source_late_barrier = source_late.seal().expect("source-late barrier");
        assert!(
            source_late_barrier
                .dispatch_outcome()
                .rows()
                .iter()
                .all(|row| row.key().role() != WorkUnitRole::SourceRead)
        );

        let mut late =
            dispatch_owner::LiveDispatchOwner::new_for_test(copy_progress_with_admission_claims())
                .expect("late owner");
        let source = key_for(late.progress(), 0, WorkUnitRole::SourceRead);
        let source_reserved = late.issue(source).expect("source reservation");
        let source_entered = late.enter(source_reserved).expect("source entry");
        late.complete(source_entered, evidence_for(late.progress(), source, 7))
            .expect("source completion");
        let prepare = key_for(late.progress(), 0, WorkUnitRole::Prepare);
        let prepare_reserved = late.issue(prepare).expect("prepare reservation");
        let prepare_entered = late.enter(prepare_reserved).expect("prepare entry");
        let before_steps = late.progress().steps().to_vec();
        late.request_cancellation(CancellationRequestObservation::new(1703).expect("cancel"))
            .expect("late cancellation");
        let context = late
            .retire_entered_completed(prepare_entered, evidence_for(late.progress(), prepare, 7))
            .expect("late completion");
        assert_eq!(context.execution_epoch(), late.progress().execution_epoch());
        assert_eq!(late.progress().steps(), before_steps.as_slice());
        assert_eq!(
            late.progress()
                .step_for(prepare)
                .expect("prepare step")
                .status(),
            ProgressStepStatus::Pending
        );
        let late_barrier = late.seal().expect("late barrier");
        assert!(
            late_barrier
                .dispatch_outcome()
                .rows()
                .iter()
                .find(|row| row.key() == prepare)
                .is_some_and(|row| {
                    matches!(
                        row.outcome(),
                        CancellationDispatchOutcome::ObservedAfterCancellation { .. }
                    )
                })
        );
        assert!(
            late_barrier
                .dispatch_outcome()
                .rows()
                .iter()
                .all(|row| row.key() != source)
        );
    }

    #[test]
    fn live_dispatch_owner_rejects_cross_owner_and_attempt_overflow_atomically() {
        let mut owner =
            dispatch_owner::LiveDispatchOwner::new_for_test(copy_progress_with_admission_claims())
                .expect("owner");
        let source = key_for(owner.progress(), 0, WorkUnitRole::SourceRead);
        let reserved = owner.issue(source).expect("source reservation");
        let mut foreign =
            dispatch_owner::LiveDispatchOwner::new_for_test(copy_progress_with_admission_claims())
                .expect("foreign owner");
        let failure = foreign.enter(reserved).expect_err("cross-owner entry");
        assert_eq!(
            failure.error(),
            &dispatch_owner::DispatchOwnerError::PermitOwnerMismatch
        );
        let (error, reserved) = failure.into_parts();
        assert_eq!(
            error,
            dispatch_owner::DispatchOwnerError::PermitOwnerMismatch
        );
        assert!(foreign.ledger_state(source).is_none());
        owner
            .return_reserved(reserved)
            .expect("recover reservation");

        let mut overflow =
            dispatch_owner::LiveDispatchOwner::new_for_test(copy_progress_with_admission_claims())
                .expect("overflow owner");
        let source = key_for(overflow.progress(), 0, WorkUnitRole::SourceRead);
        let source_reserved = overflow.issue(source).expect("source reservation");
        let source_entered = overflow.enter(source_reserved).expect("source entry");
        overflow
            .complete(source_entered, evidence_for(overflow.progress(), source, 7))
            .expect("source completion");
        let prepare = key_for(overflow.progress(), 0, WorkUnitRole::Prepare);
        overflow.set_next_attempt_for_test(u64::MAX);
        let before = overflow.progress().clone();
        assert!(matches!(
            overflow.issue(prepare),
            Err(dispatch_owner::DispatchOwnerError::AttemptIdExhausted)
        ));
        assert_eq!(overflow.progress(), &before);
        assert!(overflow.ledger_state(prepare).is_none());

        let mut generation_overflow =
            dispatch_owner::LiveDispatchOwner::new_for_test(copy_progress_with_admission_claims())
                .expect("generation-overflow owner");
        generation_overflow.set_recovery_generation_for_test(u64::MAX);
        generation_overflow
            .request_cancellation(CancellationRequestObservation::new(17018).expect("cancel"))
            .expect("generation-overflow cancellation");
        assert!(matches!(
            generation_overflow.seal(),
            Err(dispatch_owner::DispatchOwnerError::RecoveryGenerationExhausted)
        ));
        assert_eq!(
            generation_overflow.lifecycle(),
            dispatch_owner::DispatchOwnerLifecycle::CancelDraining
        );
    }

    #[test]
    fn live_dispatch_owner_barrier_accepts_nonterminal_after_effect_and_cleanup() {
        let mut before_publish =
            dispatch_owner::LiveDispatchOwner::new_for_test(copy_progress_with_admission_claims())
                .expect("before-publish owner");
        let before_steps = before_publish.progress().steps().to_vec();
        before_publish
            .request_cancellation(CancellationRequestObservation::new(17030).expect("cancel"))
            .expect("before-publish cancellation");
        assert_eq!(
            before_publish
                .progress()
                .cancellation_inventory()
                .expect("cancellation inventory")
                .route(),
            CancellationRoute::BeforePublish
        );
        let before_barrier = before_publish.seal().expect("before-publish barrier");
        let (before_recovery, before_source, before_coordinator) = empty_after_effect_inputs(
            before_publish.progress(),
            before_barrier.dispatch_outcome().clone(),
        );
        before_publish
            .accept_after_effect_reconciliation(
                &before_barrier,
                before_publish.progress().recovery_generation(),
                before_source.clone(),
                before_coordinator.clone(),
            )
            .expect("before-publish after-effect acceptance");
        assert_eq!(before_publish.progress().recovery_generation(), 1);
        assert_eq!(before_publish.progress().steps(), before_steps.as_slice());
        let (before_resource, before_namespace) = owner_pending_cleanup_observation_records(
            &before_publish,
            &before_recovery,
            before_barrier.dispatch_outcome(),
            1,
        );
        before_publish
            .accept_cancellation_cleanup_observations(
                &before_barrier,
                before_publish.progress().recovery_generation(),
                before_resource.clone(),
                before_namespace.clone(),
            )
            .expect("before-publish cleanup acceptance");
        assert_eq!(before_publish.progress().recovery_generation(), 2);
        assert_eq!(
            before_publish.progress().disposition(),
            FullProgressDisposition::CancelRequestedBeforePublish
        );
        let before_replay_generation = before_publish.progress().recovery_generation();
        before_publish
            .accept_cancellation_cleanup_observations(
                &before_barrier,
                before_replay_generation,
                before_resource,
                before_namespace,
            )
            .expect("before-publish cleanup replay");
        assert_eq!(
            before_publish.progress().recovery_generation(),
            before_replay_generation
        );
        assert_eq!(
            before_publish.lifecycle(),
            dispatch_owner::DispatchOwnerLifecycle::Sealed
        );
        assert!(matches!(
            before_publish.issue(key_for(before_publish.progress(), 0, WorkUnitRole::Prepare)),
            Err(dispatch_owner::DispatchOwnerError::IssuanceClosed)
        ));

        let mut after_publish =
            dispatch_owner::LiveDispatchOwner::new_for_test(copy_progress_with_admission_claims())
                .expect("after-publish owner");
        owner_publish_prefix(&mut after_publish);
        let after_steps = after_publish.progress().steps().to_vec();
        let after_claims = after_publish.progress().admission_claims().cloned();
        after_publish
            .request_cancellation(CancellationRequestObservation::new(17031).expect("cancel"))
            .expect("after-publish cancellation");
        assert_eq!(
            after_publish
                .progress()
                .cancellation_inventory()
                .expect("cancellation inventory")
                .route(),
            CancellationRoute::AfterPublish
        );
        let after_barrier = after_publish.seal().expect("after-publish barrier");
        let (after_recovery, after_source, after_coordinator) = empty_after_effect_inputs(
            after_publish.progress(),
            after_barrier.dispatch_outcome().clone(),
        );
        after_publish
            .accept_after_effect_reconciliation(
                &after_barrier,
                after_publish.progress().recovery_generation(),
                after_source.clone(),
                after_coordinator.clone(),
            )
            .expect("after-publish after-effect acceptance");
        assert_eq!(after_publish.progress().recovery_generation(), 1);
        assert_eq!(after_publish.progress().steps(), after_steps.as_slice());
        assert_eq!(
            after_publish.progress().admission_claims(),
            after_claims.as_ref()
        );
        let after_replay_generation = after_publish.progress().recovery_generation();
        after_publish
            .accept_after_effect_reconciliation(
                &after_barrier,
                after_replay_generation,
                after_source,
                after_coordinator,
            )
            .expect("after-publish after-effect replay");
        assert_eq!(
            after_publish.progress().recovery_generation(),
            after_replay_generation
        );
        let (after_resource, after_namespace) = owner_pending_cleanup_observation_records(
            &after_publish,
            &after_recovery,
            after_barrier.dispatch_outcome(),
            1,
        );
        after_publish
            .accept_cancellation_cleanup_observations(
                &after_barrier,
                after_publish.progress().recovery_generation(),
                after_resource.clone(),
                after_namespace.clone(),
            )
            .expect("after-publish cleanup acceptance");
        let cleanup_replay_generation = after_publish.progress().recovery_generation();
        after_publish
            .accept_cancellation_cleanup_observations(
                &after_barrier,
                cleanup_replay_generation,
                after_resource,
                after_namespace,
            )
            .expect("after-publish cleanup replay");
        assert_eq!(
            after_publish.progress().recovery_generation(),
            cleanup_replay_generation
        );
        assert_eq!(after_publish.progress().steps(), after_steps.as_slice());
        assert_eq!(
            after_publish.progress().admission_claims(),
            after_claims.as_ref()
        );
        assert_eq!(
            after_publish.lifecycle(),
            dispatch_owner::DispatchOwnerLifecycle::Sealed
        );
        assert!(matches!(
            after_publish.issue(key_for(
                after_publish.progress(),
                0,
                WorkUnitRole::SourceRead
            )),
            Err(dispatch_owner::DispatchOwnerError::IssuanceClosed)
        ));
    }

    #[test]
    fn live_dispatch_owner_guard_wrappers_reject_foreign_and_stale_inputs_atomically() {
        let mut owner =
            dispatch_owner::LiveDispatchOwner::new_for_test(copy_progress_with_admission_claims())
                .expect("owner");
        owner
            .request_cancellation(CancellationRequestObservation::new(17032).expect("cancel"))
            .expect("owner cancellation");
        let barrier = owner.seal().expect("owner barrier");
        let (recovery, source, coordinator) =
            empty_after_effect_inputs(owner.progress(), barrier.dispatch_outcome().clone());
        let before_cleanup = owner.progress().clone();
        let (resource, namespace) = owner_pending_cleanup_observation_records(
            &owner,
            &recovery,
            barrier.dispatch_outcome(),
            1,
        );
        assert_eq!(
            owner.accept_cancellation_cleanup_observations(
                &barrier,
                owner.progress().recovery_generation(),
                resource.clone(),
                namespace.clone(),
            ),
            Err(dispatch_owner::DispatchOwnerError::Progress(
                WorkProgressError::CancellationCleanupAfterEffectMissing
            ))
        );
        assert_eq!(owner.progress(), &before_cleanup);

        let mut foreign =
            dispatch_owner::LiveDispatchOwner::new_for_test(copy_progress_with_admission_claims())
                .expect("foreign owner");
        foreign
            .request_cancellation(CancellationRequestObservation::new(17033).expect("cancel"))
            .expect("foreign cancellation");
        let foreign_barrier = foreign.seal().expect("foreign barrier");
        let before_foreign = owner.progress().clone();
        assert_eq!(
            owner.accept_after_effect_reconciliation(
                &foreign_barrier,
                owner.progress().recovery_generation(),
                source.clone(),
                coordinator.clone(),
            ),
            Err(dispatch_owner::DispatchOwnerError::PermitOwnerMismatch)
        );
        assert_eq!(owner.progress(), &before_foreign);

        owner
            .accept_after_effect_reconciliation(
                &barrier,
                owner.progress().recovery_generation(),
                source,
                coordinator,
            )
            .expect("owner after-effect acceptance");
        let before_stale_generation = owner.progress().clone();
        assert_eq!(
            owner.accept_cancellation_cleanup_observations(
                &barrier,
                0,
                resource.clone(),
                namespace.clone(),
            ),
            Err(dispatch_owner::DispatchOwnerError::Progress(
                WorkProgressError::CancellationCleanupGenerationMismatch {
                    expected: 1,
                    actual: 0,
                }
            ))
        );
        assert_eq!(owner.progress(), &before_stale_generation);

        let mut other = dispatch_owner::LiveDispatchOwner::new_for_test(
            copy_progress_with_two_admission_claims(),
        )
        .expect("other owner");
        other
            .request_cancellation(CancellationRequestObservation::new(17034).expect("cancel"))
            .expect("other cancellation");
        let other_barrier = other.seal().expect("other barrier");
        let (other_recovery, other_source, other_coordinator) =
            empty_after_effect_inputs(other.progress(), other_barrier.dispatch_outcome().clone());
        other
            .accept_after_effect_reconciliation(
                &other_barrier,
                other.progress().recovery_generation(),
                other_source,
                other_coordinator,
            )
            .expect("other after-effect acceptance");
        let (other_resource, other_namespace) = owner_pending_cleanup_observation_records(
            &other,
            &other_recovery,
            other_barrier.dispatch_outcome(),
            1,
        );
        let before_bad_lineage = owner.progress().clone();
        assert!(
            owner
                .accept_cancellation_cleanup_observations(
                    &barrier,
                    owner.progress().recovery_generation(),
                    other_resource,
                    other_namespace,
                )
                .is_err()
        );
        assert_eq!(owner.progress(), &before_bad_lineage);
    }

    #[test]
    fn full_progress_snapshot_captures_initial_state_and_roundtrips_exactly() {
        let progress = copy_progress_with_admission_claims();
        let before = progress.clone();
        let intent = copy_snapshot_intent(&progress);
        let snapshot = progress
            .capture_full_progress_snapshot(&intent)
            .expect("initial full-progress snapshot");

        let encoded = serde_json::to_string(&snapshot).expect("snapshot JSON");
        let decoded: FullProgressSnapshotObservation =
            serde_json::from_str(&encoded).expect("snapshot JSON roundtrip");
        assert_eq!(decoded, snapshot);
        decoded
            .validate_base()
            .expect("decoded snapshot base validation");
        assert_eq!(progress, before);

        let mut unknown = serde_json::to_value(&snapshot).expect("snapshot value");
        unknown
            .as_object_mut()
            .expect("snapshot object")
            .insert("unknown_snapshot_field".to_owned(), json!(true));
        assert!(serde_json::from_value::<FullProgressSnapshotObservation>(unknown).is_err());
    }

    #[test]
    fn full_progress_codec_roundtrips_representative_snapshot_states() {
        let roundtrip = |snapshot: FullProgressSnapshotObservation| {
            let record =
                super::super::full_progress_codec::StoredFullProgressRecord::new(snapshot.clone())
                    .expect("validated full-progress record");
            assert_eq!(record.record_kind(), "full_progress");
            assert_eq!(record.schema_version(), 1);
            let bytes = super::super::full_progress_codec::encode(&record)
                .expect("full-progress record encodes");
            assert!(
                bytes.len() <= super::super::full_progress_codec::FULL_PROGRESS_MAX_RECORD_BYTES
            );
            let decoded = super::super::full_progress_codec::decode(&bytes)
                .expect("full-progress record decodes");
            assert_eq!(decoded.snapshot(), &snapshot);
            decoded
        };

        let initial = copy_progress_with_admission_claims();
        let initial_snapshot = initial
            .capture_full_progress_snapshot(&copy_snapshot_intent(&initial))
            .expect("initial snapshot");
        roundtrip(initial_snapshot);

        let mut reacquired = copy_progress_with_admission_claims();
        let source_key = key_for(&reacquired, 0, WorkUnitRole::SourceRead);
        apply_role(&mut reacquired, 0, WorkUnitRole::SourceRead);
        reacquired
            .reacquire_source(source_reacquisition_authority(
                &reacquired,
                source_key.participant_id(),
                7,
                2,
                0,
            ))
            .expect("source reacquisition");
        let reacquired_snapshot = reacquired
            .capture_full_progress_snapshot(&copy_snapshot_intent(&reacquired))
            .expect("reacquired snapshot");
        roundtrip(reacquired_snapshot);

        let mut retry = copy_progress_with_admission_claims();
        retry
            .request_retry(
                RetryObservation::new(
                    1,
                    100,
                    200,
                    RetryLeaseId::new("codec-retry").expect("retry lease"),
                    300,
                    RetryErrorClass::TransientAvailability,
                    "codec-test",
                    None,
                )
                .expect("retry observation"),
            )
            .expect("retry request");
        roundtrip(
            retry
                .capture_full_progress_snapshot(&copy_snapshot_intent(&retry))
                .expect("retry snapshot"),
        );

        let mut audited = copy_progress_with_admission_claims();
        publish_prefix(&mut audited);
        audited
            .require_audit(ProgressAuditObservation::new(
                ProgressAuditReason::RevisionGap,
            ))
            .expect("audit request");
        roundtrip(
            audited
                .capture_full_progress_snapshot(&copy_snapshot_intent(&audited))
                .expect("audit snapshot"),
        );

        let mut cancelled = copy_progress_with_admission_claims();
        cancelled
            .request_cancellation(CancellationRequestObservation::new(18121).expect("cancel"))
            .expect("cancellation request");
        accept_empty_after_effect_reconciliation(&mut cancelled);
        accept_pending_cleanup_observations(&mut cancelled);
        roundtrip(
            cancelled
                .capture_full_progress_snapshot(&copy_snapshot_intent(&cancelled))
                .expect("cancellation and cleanup snapshot"),
        );

        let mut deferred = copy_progress_with_admission_claims_and_optional();
        complete_required(&mut deferred);
        let artifact = deferred
            .steps()
            .iter()
            .find(|step| step.requirement() == ParticipantRequirement::OptionalRebuildable)
            .map(WorkProgressStep::key)
            .expect("optional artifact");
        apply_readiness_deferral(&mut deferred, &[artifact], 1);
        let (participants, source, _filesystem, source_id) = valid_topology(true);
        let args = copy_args_at(source, source_id, "copy.wav");
        roundtrip(
            deferred
                .capture_full_progress_snapshot(&snapshot_intent_for(&deferred, args, participants))
                .expect("deferred snapshot"),
        );
    }

    #[test]
    fn full_progress_codec_rejects_malformed_headers_nested_data_and_tampering() {
        let progress = copy_progress_with_admission_claims();
        let snapshot = progress
            .capture_full_progress_snapshot(&copy_snapshot_intent(&progress))
            .expect("snapshot");
        let record = super::super::full_progress_codec::StoredFullProgressRecord::new(snapshot)
            .expect("record");
        let encoded = super::super::full_progress_codec::encode(&record).expect("encode");
        let value: serde_json::Value = serde_json::from_slice(&encoded).expect("value");

        let mut wrong_kind = value.clone();
        wrong_kind["record_kind"] = json!("legacy");
        assert!(matches!(
            super::super::full_progress_codec::decode(
                &serde_json::to_vec(&wrong_kind).expect("wrong kind")
            ),
            Err(super::super::full_progress_codec::FullProgressCodecError::WrongRecordKind { .. })
        ));

        let mut unsupported = value.clone();
        unsupported["schema_version"] = json!(99);
        assert!(matches!(
            super::super::full_progress_codec::decode(
                &serde_json::to_vec(&unsupported).expect("unsupported version")
            ),
            Err(
                super::super::full_progress_codec::FullProgressCodecError::UnsupportedVersion {
                    version: 99
                }
            )
        ));

        let mut missing_kind = value.clone();
        missing_kind
            .as_object_mut()
            .expect("envelope object")
            .remove("record_kind");
        assert!(matches!(
            super::super::full_progress_codec::decode(
                &serde_json::to_vec(&missing_kind).expect("missing kind")
            ),
            Err(super::super::full_progress_codec::FullProgressCodecError::Malformed { .. })
        ));

        let mut unknown_nested = value.clone();
        unknown_nested["snapshot"]["intent"]["unexpected"] = json!(true);
        assert!(matches!(
            super::super::full_progress_codec::decode(
                &serde_json::to_vec(&unknown_nested).expect("unknown nested")
            ),
            Err(super::super::full_progress_codec::FullProgressCodecError::Malformed { .. })
        ));

        let mut unknown_root = value.clone();
        unknown_root["unexpected"] = json!(true);
        assert!(matches!(
            super::super::full_progress_codec::decode(
                &serde_json::to_vec(&unknown_root).expect("unknown root")
            ),
            Err(super::super::full_progress_codec::FullProgressCodecError::Malformed { .. })
        ));

        let mut tampered = value.clone();
        tampered["snapshot"]["phase"] = json!("Terminal");
        let tampered_record = serde_json::from_value::<
            super::super::full_progress_codec::StoredFullProgressRecord,
        >(tampered.clone())
        .expect("tampered envelope shape");
        assert!(matches!(
            super::super::full_progress_codec::encode(&tampered_record),
            Err(super::super::full_progress_codec::FullProgressCodecError::InvalidSnapshot { .. })
        ));
        assert!(matches!(
            super::super::full_progress_codec::decode(
                &serde_json::to_vec(&tampered).expect("tampered snapshot")
            ),
            Err(super::super::full_progress_codec::FullProgressCodecError::InvalidSnapshot { .. })
        ));

        let mut trailing = encoded.clone();
        trailing.extend_from_slice(b" {}");
        assert!(matches!(
            super::super::full_progress_codec::decode(&trailing),
            Err(super::super::full_progress_codec::FullProgressCodecError::Malformed { .. })
        ));

        let duplicate_root = br#"{"record_kind":"full_progress","record_kind":"full_progress","schema_version":1,"snapshot":{}}"#;
        assert!(matches!(
            super::super::full_progress_codec::decode(duplicate_root),
            Err(super::super::full_progress_codec::FullProgressCodecError::Malformed { ref reason })
                if reason.contains("duplicate")
        ));
        let duplicate_nested = br#"{"record_kind":"full_progress","schema_version":1,"snapshot":{"intent":{},"intent":{}}}"#;
        assert!(matches!(
            super::super::full_progress_codec::decode(duplicate_nested),
            Err(super::super::full_progress_codec::FullProgressCodecError::Malformed { ref reason })
                if reason.contains("duplicate")
        ));

        let mut deep = String::from("{}");
        for _ in 0..=super::super::full_progress_codec::FULL_PROGRESS_MAX_JSON_DEPTH {
            deep = format!("[{deep}]");
        }
        let deep = format!(
            "{{\"record_kind\":\"full_progress\",\"schema_version\":1,\"snapshot\":{deep}}}"
        );
        assert!(matches!(
            super::super::full_progress_codec::decode(deep.as_bytes()),
            Err(
                super::super::full_progress_codec::FullProgressCodecError::JsonDepthExceeded { .. }
            )
        ));

        let oversized =
            vec![b'{'; super::super::full_progress_codec::FULL_PROGRESS_MAX_RECORD_BYTES + 1];
        assert!(matches!(
            super::super::full_progress_codec::decode(&oversized),
            Err(super::super::full_progress_codec::FullProgressCodecError::InputOversize { .. })
        ));
    }

    #[test]
    fn full_progress_codec_is_separate_from_legacy_codec_and_caps_writes() {
        let progress = copy_progress_with_admission_claims();
        let snapshot = progress
            .capture_full_progress_snapshot(&copy_snapshot_intent(&progress))
            .expect("snapshot");
        let record = super::super::full_progress_codec::StoredFullProgressRecord::new(snapshot)
            .expect("record");
        let full_bytes = super::super::full_progress_codec::encode(&record).expect("encode");
        assert!(matches!(
            super::super::codec::decode(&full_bytes),
            Err(super::super::codec::DecodeError::Malformed { .. })
        ));

        let legacy_bytes = br#"{
  "schema_version": 1,
  "operation_id": "01234567-89ab-cdef-0123-456789abcdef",
  "intent": {"actor": "User", "kind": "FileHistory", "label": "legacy fixture"},
  "phase": "IntentDurable",
  "disposition": "RetryPending",
  "payload": {"legacy": true},
  "capacity_plan": null,
  "prepared": null,
  "staged": null,
  "published": null,
  "replacement_qualification": null,
  "created_unix_ms": 11,
  "updated_unix_ms": 12
}"#;
        assert!(matches!(
            super::super::full_progress_codec::decode(legacy_bytes),
            Err(super::super::full_progress_codec::FullProgressCodecError::Malformed { .. })
        ));

        assert!(matches!(
            super::super::full_progress_codec::encode_for_test_with_limit(&record, 16),
            Err(
                super::super::full_progress_codec::FullProgressCodecError::OutputOversize {
                    maximum: 16,
                    ..
                }
            )
        ));
    }

    #[test]
    fn full_progress_snapshot_validates_completed_history_after_reacquisition() {
        let mut progress = copy_progress_with_admission_claims();
        let source_key = key_for(&progress, 0, WorkUnitRole::SourceRead);
        apply_role(&mut progress, 0, WorkUnitRole::SourceRead);
        progress
            .reacquire_source(source_reacquisition_authority(
                &progress,
                source_key.participant_id(),
                7,
                2,
                0,
            ))
            .expect("source reacquisition");

        let snapshot = progress
            .capture_full_progress_snapshot(&copy_snapshot_intent(&progress))
            .expect("reacquired full-progress snapshot");
        assert_eq!(
            snapshot
                .steps()
                .iter()
                .find(|step| step.key() == source_key)
                .expect("source row")
                .status(),
            ProgressStepStatus::Completed
        );
        let decoded: FullProgressSnapshotObservation =
            serde_json::from_str(&serde_json::to_string(&snapshot).expect("snapshot JSON"))
                .expect("reacquired snapshot JSON");
        decoded
            .validate_base()
            .expect("historical completion remains valid");
    }

    #[test]
    fn full_progress_snapshot_rejects_split_current_source_fences() {
        let mut progress = copy_progress_with_admission_claims();
        let source_participant = progress
            .scopes
            .iter()
            .find(|scope| scope.kind() == ParticipantKind::SourceDatabase)
            .expect("source participant")
            .participant_id();
        progress
            .reacquire_source(source_reacquisition_authority(
                &progress,
                source_participant,
                7,
                2,
                0,
            ))
            .expect("coherent source reacquisition");

        let coherent = progress
            .capture_full_progress_snapshot(&copy_snapshot_intent(&progress))
            .expect("coherent reacquired snapshot");
        coherent.validate_base().expect("coherent current fences");

        let mut split = serde_json::to_value(&coherent).expect("snapshot value");
        let scopes = split
            .get_mut("scopes")
            .and_then(serde_json::Value::as_array_mut)
            .expect("snapshot scopes");
        let source_fenced = scopes
            .iter()
            .enumerate()
            .filter(|(_, scope)| {
                scope
                    .get("source_fence")
                    .is_some_and(|fence| !fence.is_null())
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        assert!(
            source_fenced.len() >= 2,
            "expected related source-fenced scopes"
        );
        let split_index = source_fenced[1];
        let split_fence = scopes[split_index]
            .get_mut("source_fence")
            .expect("source fence")
            .as_object_mut()
            .expect("source fence object");
        split_fence.insert("source_revision".to_owned(), json!(8));
        split_fence.insert("lease_epoch".to_owned(), json!(3));

        let split: FullProgressSnapshotObservation =
            serde_json::from_value(split).expect("split fence shape");
        assert!(matches!(
            split.validate_base(),
            Err(FullProgressSnapshotObservationError::ScopeStableFenceMismatch { .. })
        ));
    }

    #[test]
    fn full_progress_snapshot_accepts_initial_artifact_revision_split_after_retry_resume() {
        let (progress, intent) = copy_progress_with_initial_artifact_fence_split(1, 0);
        let snapshot = progress
            .capture_full_progress_snapshot(&intent)
            .expect("initial artifact revision split snapshot");
        let decoded: FullProgressSnapshotObservation =
            serde_json::from_str(&serde_json::to_string(&snapshot).expect("split snapshot JSON"))
                .expect("split snapshot roundtrip");
        decoded
            .validate_base()
            .expect("initial split remains valid observation");

        let mut resumed = progress.clone();
        let retry_observation = RetryObservation::new(
            1,
            0,
            1,
            RetryLeaseId::new("snapshot-split-retry").expect("retry lease"),
            2,
            RetryErrorClass::TransientContention,
            "snapshot split retry",
            None,
        )
        .expect("retry observation");
        resumed
            .request_retry(retry_observation.clone())
            .expect("request retry");
        resumed
            .resume_retry(VerifiedRetryResume {
                operation_id: resumed.operation_id,
                plan: resumed.plan.clone(),
                retry_observation,
                now_unix_ms: 1,
                expected_execution_epoch: 0,
            })
            .expect("resume retry");
        assert_eq!(resumed.execution_epoch(), 1);
        let resumed_snapshot = resumed
            .capture_full_progress_snapshot(&intent)
            .expect("unchanged initial split after retry resume");
        resumed_snapshot
            .validate_base()
            .expect("resumed split remains valid observation");
    }

    #[test]
    fn full_progress_snapshot_rejects_rebind_at_existing_initial_max_lease() {
        let (progress, intent) = copy_progress_with_initial_artifact_fence_split(0, 1);
        let mut resumed = progress.clone();
        let retry_observation = RetryObservation::new(
            1,
            0,
            1,
            RetryLeaseId::new("snapshot-lease-split-retry").expect("retry lease"),
            2,
            RetryErrorClass::TransientContention,
            "snapshot lease split retry",
            None,
        )
        .expect("retry observation");
        resumed
            .request_retry(retry_observation.clone())
            .expect("request retry");
        resumed
            .resume_retry(VerifiedRetryResume {
                operation_id: resumed.operation_id,
                plan: resumed.plan.clone(),
                retry_observation,
                now_unix_ms: 1,
                expected_execution_epoch: 0,
            })
            .expect("resume retry");

        let snapshot = resumed
            .capture_full_progress_snapshot(&intent)
            .expect("split lease snapshot before unification");
        let mut unified = serde_json::to_value(&snapshot).expect("snapshot value");
        let scopes = unified
            .get_mut("scopes")
            .and_then(serde_json::Value::as_array_mut)
            .expect("snapshot scopes");
        let artifact_fence = scopes
            .iter()
            .find(|scope| scope["kind"] == json!("Artifact"))
            .and_then(|scope| scope.get("source_fence"))
            .expect("artifact fence")
            .clone();
        for scope in scopes {
            if scope
                .get("source_fence")
                .is_some_and(|fence| !fence.is_null())
            {
                scope["source_fence"] = artifact_fence.clone();
            }
        }
        let unified: FullProgressSnapshotObservation =
            serde_json::from_value(unified).expect("unified fence shape");
        assert!(matches!(
            unified.validate_base(),
            Err(FullProgressSnapshotObservationError::ScopeStableFenceLeaseNotAdvanced { .. })
        ));
    }

    #[test]
    fn full_progress_snapshot_retains_retry_audit_and_cancellation_observations() {
        let mut retry = copy_progress_with_admission_claims();
        retry
            .request_retry(
                RetryObservation::new(
                    1,
                    100,
                    200,
                    RetryLeaseId::new("snapshot-retry").expect("retry lease"),
                    300,
                    RetryErrorClass::TransientAvailability,
                    "snapshot-test",
                    None,
                )
                .expect("retry observation"),
            )
            .expect("retry request");
        let retry_snapshot = retry
            .capture_full_progress_snapshot(&copy_snapshot_intent(&retry))
            .expect("retry snapshot");
        assert!(retry_snapshot.retry_observation().is_some());
        assert!(retry_snapshot.last_retry_observation().is_some());

        let mut audit = copy_progress_with_admission_claims();
        publish_prefix(&mut audit);
        audit
            .require_audit(ProgressAuditObservation::new(
                ProgressAuditReason::RevisionGap,
            ))
            .expect("audit request");
        let audit_snapshot = audit
            .capture_full_progress_snapshot(&copy_snapshot_intent(&audit))
            .expect("audit snapshot");
        assert!(audit_snapshot.audit_observation().is_some());
        assert_eq!(audit_snapshot.audit_generation(), 1);

        let mut cancelled = copy_progress_with_admission_claims();
        cancelled
            .request_cancellation(CancellationRequestObservation::new(18101).expect("cancel"))
            .expect("cancellation request");
        accept_empty_after_effect_reconciliation(&mut cancelled);
        accept_pending_cleanup_observations(&mut cancelled);
        let before = cancelled.clone();
        let cancellation_snapshot = cancelled
            .capture_full_progress_snapshot(&copy_snapshot_intent(&cancelled))
            .expect("cancellation snapshot");
        let decoded: FullProgressSnapshotObservation = serde_json::from_str(
            &serde_json::to_string(&cancellation_snapshot).expect("cancellation JSON"),
        )
        .expect("cancellation snapshot JSON");
        assert_eq!(decoded, cancellation_snapshot);
        assert!(decoded.after_effect_reconciliation().is_some());
        assert!(decoded.cancellation_cleanup_observations().is_some());
        assert_eq!(cancelled, before);
    }

    #[test]
    fn full_progress_snapshot_validates_retained_after_effect_headers_and_history() {
        let mut progress = copy_progress_with_admission_claims();
        progress
            .request_cancellation(CancellationRequestObservation::new(18106).expect("cancel"))
            .expect("cancellation request");
        accept_empty_after_effect_reconciliation(&mut progress);
        let initial_snapshot = progress
            .capture_full_progress_snapshot(&copy_snapshot_intent(&progress))
            .expect("initial after-effect snapshot");
        initial_snapshot
            .validate_base()
            .expect("initial after-effect snapshot validates");

        // A later cleanup observation advances the global generation while the accepted
        // after-effect generation remains historical and must still validate.
        accept_pending_cleanup_observations(&mut progress);
        let cleanup_snapshot = progress
            .capture_full_progress_snapshot(&copy_snapshot_intent(&progress))
            .expect("cleanup snapshot");
        cleanup_snapshot
            .validate_base()
            .expect("cleanup snapshot retains valid after-effect state");

        // Reacquisition changes the current epoch and scopes, but the accepted after-effect
        // records retain their original observation epoch and historical context.
        let source_key = key_for(&progress, 0, WorkUnitRole::SourceRead);
        progress
            .reacquire_source(source_reacquisition_authority(
                &progress,
                source_key.participant_id(),
                7,
                2,
                progress.execution_epoch(),
            ))
            .expect("reacquire source after accepted after-effect");
        let reacquired_snapshot = progress
            .capture_full_progress_snapshot(&copy_snapshot_intent(&progress))
            .expect("reacquired after-effect snapshot");
        assert_eq!(
            reacquired_snapshot
                .after_effect_reconciliation()
                .expect("retained after-effect")
                .execution_epoch(),
            0
        );
        reacquired_snapshot
            .validate_base()
            .expect("historical after-effect snapshot validates");

        // A later current-epoch after-effect refresh leaves the cleanup observation stale but
        // valid. Snapshot validation uses the cleanup record's own historical lineage and
        // requires the newer dispatch to refine that retained dispatch.
        refresh_after_effect_headers_after_reacquisition(&mut progress);
        let refreshed_snapshot = progress
            .capture_full_progress_snapshot(&copy_snapshot_intent(&progress))
            .expect("refreshed after-effect snapshot");
        refreshed_snapshot
            .validate_base()
            .expect("stale cleanup remains valid after after-effect refresh");
        assert_eq!(
            refreshed_snapshot
                .cancellation_cleanup_observations()
                .expect("retained cleanup")
                .after_effect_generation(),
            1
        );
        assert_eq!(
            refreshed_snapshot
                .after_effect_reconciliation()
                .expect("refreshed after-effect")
                .recovery_generation(),
            3
        );
    }

    #[test]
    fn full_progress_snapshot_validates_observed_cleanup_context_after_reacquisition() {
        let mut progress = copy_progress_with_admission_claims();
        progress
            .request_cancellation(CancellationRequestObservation::new(18118).expect("cancel"))
            .expect("cancellation request");
        accept_empty_after_effect_reconciliation(&mut progress);
        let (pending_resource, pending_namespace) =
            accept_pending_cleanup_observations(&mut progress);
        let resource_key = pending_resource.manifest().keys()[0];
        let resource_context = CompletionContext::new(
            progress.execution_epoch(),
            progress
                .scope_for(resource_key)
                .expect("resource scope")
                .clone(),
        );
        let observed_resource = CancellationResourceCleanupObservationRecord::new(
            pending_resource.manifest().clone(),
            progress.execution_epoch(),
            2,
            pending_resource
                .manifest()
                .keys()
                .iter()
                .map(|key| {
                    let outcome = if *key == resource_key {
                        CancellationResourceCleanupOutcome::Preserved {
                            owner_revision: 1,
                            context: resource_context.clone(),
                        }
                    } else {
                        CancellationResourceCleanupOutcome::Pending
                    };
                    CancellationResourceCleanupObservationRow::new(*key, outcome)
                })
                .collect(),
        )
        .expect("observed resource cleanup");
        progress
            .accept_cancellation_cleanup_observations(cleanup_observation_authority(
                &progress,
                observed_resource,
                pending_namespace,
                progress.recovery_generation(),
            ))
            .expect("accept observed cleanup");

        let snapshot = progress
            .capture_full_progress_snapshot(&copy_snapshot_intent(&progress))
            .expect("observed cleanup snapshot");
        snapshot
            .validate_base()
            .expect("observed cleanup snapshot validates");

        let mut zero_owner_revision = serde_json::to_value(&snapshot).expect("snapshot");
        zero_owner_revision["cancellation_cleanup_observations"]["resource_cleanup_observation"]
            ["rows"][0]["outcome"]["Preserved"]["owner_revision"] = json!(0);
        let zero_owner_revision: FullProgressSnapshotObservation =
            serde_json::from_value(zero_owner_revision).expect("owner revision shape");
        assert!(matches!(
            zero_owner_revision.validate_base(),
            Err(
                FullProgressSnapshotObservationError::CancellationCleanupValidation {
                    source: WorkProgressError::CancellationResourceCleanupObservation(_),
                }
            )
        ));

        let mut future_context = serde_json::to_value(&snapshot).expect("snapshot");
        future_context["cancellation_cleanup_observations"]["resource_cleanup_observation"]["rows"]
            [0]["outcome"]["Preserved"]["context"]["execution_epoch"] = json!(1);
        let future_context: FullProgressSnapshotObservation =
            serde_json::from_value(future_context).expect("context epoch shape");
        assert!(matches!(
            future_context.validate_base(),
            Err(
                FullProgressSnapshotObservationError::CancellationCleanupValidation {
                    source: WorkProgressError::CancellationResourceCleanupObservation(_),
                }
            )
        ));

        let source_key = key_for(&progress, 0, WorkUnitRole::SourceRead);
        progress
            .reacquire_source(source_reacquisition_authority(
                &progress,
                source_key.participant_id(),
                7,
                2,
                progress.execution_epoch(),
            ))
            .expect("reacquire after observed cleanup");
        let reacquired_snapshot = progress
            .capture_full_progress_snapshot(&copy_snapshot_intent(&progress))
            .expect("reacquired observed cleanup snapshot");
        reacquired_snapshot
            .validate_base()
            .expect("historical observed cleanup remains valid");
        let observed_context = reacquired_snapshot
            .cancellation_cleanup_observations()
            .expect("cleanup observations")
            .resource_cleanup_observation()
            .rows()
            .iter()
            .find_map(|row| match row.outcome() {
                CancellationResourceCleanupOutcome::Preserved { context, .. } => Some(context),
                _ => None,
            })
            .expect("observed historical context");
        assert_eq!(observed_context.execution_epoch(), 0);
    }

    #[test]
    fn full_progress_snapshot_rejects_retained_after_effect_corruption() {
        let mut progress = copy_progress_with_admission_claims();
        progress
            .request_cancellation(CancellationRequestObservation::new(18107).expect("cancel"))
            .expect("cancellation request");
        accept_empty_after_effect_reconciliation(&mut progress);
        let snapshot = progress
            .capture_full_progress_snapshot(&copy_snapshot_intent(&progress))
            .expect("after-effect snapshot");

        let mut future_epoch = serde_json::to_value(&snapshot).expect("snapshot value");
        future_epoch["after_effect_reconciliation"]["execution_epoch"] = json!(1);
        let future_epoch: FullProgressSnapshotObservation =
            serde_json::from_value(future_epoch).expect("future after-effect epoch shape");
        assert!(matches!(
            future_epoch.validate_base(),
            Err(
                FullProgressSnapshotObservationError::AfterEffectExecutionEpochFuture {
                    accepted: 1,
                    current: 0,
                }
            )
        ));

        let mut dispatch_epoch = serde_json::to_value(&snapshot).expect("snapshot value");
        dispatch_epoch["after_effect_reconciliation"]["recovery_inventory"]["dispatch_outcome"]["observation_epoch"] =
            json!(1);
        let dispatch_epoch: FullProgressSnapshotObservation =
            serde_json::from_value(dispatch_epoch).expect("dispatch epoch shape");
        assert!(matches!(
            dispatch_epoch.validate_base(),
            Err(
                FullProgressSnapshotObservationError::AfterEffectDispatchEpochMismatch {
                    accepted: 0,
                    actual: 1,
                }
            )
        ));

        let mut source_epoch = serde_json::to_value(&snapshot).expect("snapshot value");
        source_epoch["after_effect_reconciliation"]["source_closure_observation"]["observation_epoch"] =
            json!(1);
        let source_epoch: FullProgressSnapshotObservation =
            serde_json::from_value(source_epoch).expect("source epoch shape");
        assert!(matches!(
            source_epoch.validate_base(),
            Err(
                FullProgressSnapshotObservationError::AfterEffectSourceClosureEpochMismatch {
                    accepted: 0,
                    actual: 1,
                }
            )
        ));

        let mut coordinator_epoch = serde_json::to_value(&snapshot).expect("snapshot value");
        coordinator_epoch["after_effect_reconciliation"]["coordinator_closure_observation"]["observation_epoch"] =
            json!(1);
        let coordinator_epoch: FullProgressSnapshotObservation =
            serde_json::from_value(coordinator_epoch).expect("coordinator epoch shape");
        assert!(matches!(
            coordinator_epoch.validate_base(),
            Err(
                FullProgressSnapshotObservationError::AfterEffectCoordinatorClosureEpochMismatch {
                    accepted: 0,
                    actual: 1,
                }
            )
        ));

        let mut future_generation = serde_json::to_value(&snapshot).expect("snapshot value");
        future_generation["after_effect_reconciliation"]["recovery_generation"] = json!(2);
        let future_generation: FullProgressSnapshotObservation =
            serde_json::from_value(future_generation)
                .expect("future after-effect generation shape");
        assert!(matches!(
            future_generation.validate_base(),
            Err(
                FullProgressSnapshotObservationError::AfterEffectGenerationFuture {
                    accepted: 2,
                    current: 1,
                }
            )
        ));

        let mut future_dispatch_generation =
            serde_json::to_value(&snapshot).expect("snapshot value");
        future_dispatch_generation["after_effect_reconciliation"]["recovery_inventory"]["dispatch_outcome"]
            ["recovery_generation"] = json!(2);
        let future_dispatch_generation: FullProgressSnapshotObservation =
            serde_json::from_value(future_dispatch_generation)
                .expect("future dispatch generation shape");
        assert!(matches!(
            future_dispatch_generation.validate_base(),
            Err(
                FullProgressSnapshotObservationError::AfterEffectDispatchGenerationFuture {
                    accepted: 1,
                    actual: 2,
                }
            )
        ));

        let mut wrong_lineage = serde_json::to_value(&snapshot).expect("snapshot value");
        wrong_lineage["after_effect_reconciliation"]["recovery_inventory"]["frozen_inventory"]["operation_id"] =
            json!(Uuid::from_u128(18108).to_string());
        let wrong_lineage: FullProgressSnapshotObservation =
            serde_json::from_value(wrong_lineage).expect("wrong lineage shape");
        assert!(matches!(
            wrong_lineage.validate_base(),
            Err(
                FullProgressSnapshotObservationError::AfterEffectValidation {
                    source: WorkProgressError::CancellationEffectInventory(_),
                }
            )
        ));
    }

    #[test]
    fn full_progress_snapshot_rejects_retained_cleanup_lineage_corruption() {
        let mut progress = copy_progress_with_admission_claims();
        progress
            .request_cancellation(CancellationRequestObservation::new(18118).expect("cancel"))
            .expect("cancellation request");
        accept_empty_after_effect_reconciliation(&mut progress);
        accept_pending_cleanup_observations(&mut progress);
        let snapshot = progress
            .capture_full_progress_snapshot(&copy_snapshot_intent(&progress))
            .expect("cleanup snapshot");
        snapshot.validate_base().expect("valid cleanup snapshot");

        let mut resource_generation_zero = serde_json::to_value(&snapshot).expect("snapshot");
        resource_generation_zero["cancellation_cleanup_observations"]["resource_cleanup_observation"]
            ["observation_generation"] = json!(0);
        let resource_generation_zero: FullProgressSnapshotObservation =
            serde_json::from_value(resource_generation_zero).expect("resource generation shape");
        assert!(matches!(
            resource_generation_zero.validate_base(),
            Err(
                FullProgressSnapshotObservationError::CancellationCleanupValidation {
                    source: WorkProgressError::CancellationResourceCleanupObservation(_),
                }
            )
        ));

        let mut wrong_resource_key = serde_json::to_value(&snapshot).expect("snapshot");
        wrong_resource_key["cancellation_cleanup_observations"]["resource_cleanup_observation"]["rows"]
            [0]["key"]["step_ordinal"] = json!(999);
        let wrong_resource_key: FullProgressSnapshotObservation =
            serde_json::from_value(wrong_resource_key).expect("resource key shape");
        assert!(matches!(
            wrong_resource_key.validate_base(),
            Err(
                FullProgressSnapshotObservationError::CancellationCleanupValidation {
                    source: WorkProgressError::CancellationResourceCleanupObservation(_),
                }
            )
        ));

        let mut wrong_namespace_lineage = serde_json::to_value(&snapshot).expect("snapshot");
        wrong_namespace_lineage["cancellation_cleanup_observations"]["namespace_cleanup_observation"]
            ["manifest"]["cleanup_manifest"]["recovery_inventory"]["frozen_inventory"]["operation_id"] =
            json!(Uuid::from_u128(18119).to_string());
        let wrong_namespace_lineage: FullProgressSnapshotObservation =
            serde_json::from_value(wrong_namespace_lineage).expect("namespace lineage shape");
        assert!(matches!(
            wrong_namespace_lineage.validate_base(),
            Err(FullProgressSnapshotObservationError::CancellationCleanupNamespaceLineageMismatch)
        ));

        let mut future_cleanup_epoch = serde_json::to_value(&snapshot).expect("snapshot");
        future_cleanup_epoch["cancellation_cleanup_observations"]["execution_epoch"] = json!(1);
        let future_cleanup_epoch: FullProgressSnapshotObservation =
            serde_json::from_value(future_cleanup_epoch).expect("cleanup epoch shape");
        assert!(matches!(
            future_cleanup_epoch.validate_base(),
            Err(
                FullProgressSnapshotObservationError::CancellationCleanupExecutionEpochFuture {
                    cleanup: 1,
                    current: 0,
                }
            )
        ));

        let mut resource_epoch = serde_json::to_value(&snapshot).expect("snapshot");
        resource_epoch["cancellation_cleanup_observations"]["resource_cleanup_observation"]["observation_epoch"] =
            json!(1);
        let resource_epoch: FullProgressSnapshotObservation =
            serde_json::from_value(resource_epoch).expect("resource epoch shape");
        assert!(matches!(
            resource_epoch.validate_base(),
            Err(
                FullProgressSnapshotObservationError::CancellationCleanupResourceEpochMismatch {
                    cleanup: 0,
                    actual: 1,
                }
            )
        ));

        let mut dispatch_epoch = serde_json::to_value(&snapshot).expect("snapshot");
        dispatch_epoch["cancellation_cleanup_observations"]["resource_cleanup_observation"]["manifest"]
            ["cleanup_manifest"]["recovery_inventory"]["dispatch_outcome"]["observation_epoch"] =
            json!(1);
        let dispatch_epoch: FullProgressSnapshotObservation =
            serde_json::from_value(dispatch_epoch).expect("dispatch epoch shape");
        assert!(matches!(
            dispatch_epoch.validate_base(),
            Err(
                FullProgressSnapshotObservationError::CancellationCleanupDispatchEpochMismatch {
                    cleanup: 0,
                    actual: 1,
                }
            )
        ));

        let mut linked_generation_zero = serde_json::to_value(&snapshot).expect("snapshot");
        linked_generation_zero["cancellation_cleanup_observations"]["after_effect_generation"] =
            json!(0);
        let linked_generation_zero: FullProgressSnapshotObservation =
            serde_json::from_value(linked_generation_zero).expect("linked generation shape");
        assert!(matches!(
            linked_generation_zero.validate_base(),
            Err(FullProgressSnapshotObservationError::CancellationCleanupAfterEffectGenerationZero)
        ));

        let mut equal_generations = serde_json::to_value(&snapshot).expect("snapshot");
        equal_generations["cancellation_cleanup_observations"]["recovery_generation"] = json!(1);
        let equal_generations: FullProgressSnapshotObservation =
            serde_json::from_value(equal_generations).expect("equal generation shape");
        assert!(matches!(
            equal_generations.validate_base(),
            Err(
                FullProgressSnapshotObservationError::CancellationCleanupAfterEffectGenerationNotBeforeCleanup {
                    after_effect: 1,
                    cleanup: 1,
                }
            )
        ));

        let mut cleanup_generation_future = serde_json::to_value(&snapshot).expect("snapshot");
        cleanup_generation_future["cancellation_cleanup_observations"]["recovery_generation"] =
            json!(3);
        let cleanup_generation_future: FullProgressSnapshotObservation =
            serde_json::from_value(cleanup_generation_future).expect("future cleanup generation");
        assert!(matches!(
            cleanup_generation_future.validate_base(),
            Err(
                FullProgressSnapshotObservationError::CancellationCleanupGenerationFuture {
                    cleanup: 3,
                    current: 2,
                }
            )
        ));

        let mut dispatch_generation_future = serde_json::to_value(&snapshot).expect("snapshot");
        dispatch_generation_future["cancellation_cleanup_observations"]["resource_cleanup_observation"]
            ["manifest"]["cleanup_manifest"]["recovery_inventory"]["dispatch_outcome"]["recovery_generation"] =
            json!(2);
        let dispatch_generation_future: FullProgressSnapshotObservation =
            serde_json::from_value(dispatch_generation_future).expect("future dispatch generation");
        assert!(matches!(
            dispatch_generation_future.validate_base(),
            Err(
                FullProgressSnapshotObservationError::CancellationCleanupDispatchGenerationFuture {
                    dispatch: 2,
                    after_effect: 1,
                }
            )
        ));

        let mut dispatch_generation_zero = serde_json::to_value(&snapshot).expect("snapshot");
        dispatch_generation_zero["cancellation_cleanup_observations"]["resource_cleanup_observation"]
            ["manifest"]["cleanup_manifest"]["recovery_inventory"]["dispatch_outcome"]["recovery_generation"] =
            json!(0);
        let dispatch_generation_zero: FullProgressSnapshotObservation =
            serde_json::from_value(dispatch_generation_zero)
                .expect("zero dispatch generation shape");
        let dispatch_generation_zero_result = dispatch_generation_zero.validate_base();
        assert!(matches!(
            dispatch_generation_zero_result,
            Err(
                FullProgressSnapshotObservationError::CancellationCleanupValidation {
                    source: WorkProgressError::CancellationDispatchOutcome(
                        CancellationDispatchOutcomeRecordError::RecoveryGenerationZero,
                    ),
                }
            )
        ));

        let mut wrong_recovery = serde_json::to_value(&snapshot).expect("snapshot");
        wrong_recovery["cancellation_cleanup_observations"]["resource_cleanup_observation"]["manifest"]
            ["cleanup_manifest"]["recovery_inventory"]["frozen_inventory"]["operation_id"] =
            json!(Uuid::from_u128(18120).to_string());
        let wrong_recovery: FullProgressSnapshotObservation =
            serde_json::from_value(wrong_recovery).expect("wrong recovery shape");
        assert!(matches!(
            wrong_recovery.validate_base(),
            Err(
                FullProgressSnapshotObservationError::CancellationCleanupValidation {
                    source: WorkProgressError::CancellationEffectInventory(_),
                }
            )
        ));

        let source_key = key_for(&progress, 0, WorkUnitRole::SourceRead);
        progress
            .reacquire_source(source_reacquisition_authority(
                &progress,
                source_key.participant_id(),
                7,
                2,
                progress.execution_epoch(),
            ))
            .expect("reacquire before stale-generation corruption");
        refresh_after_effect_headers_after_reacquisition(&mut progress);
        let refreshed_snapshot = progress
            .capture_full_progress_snapshot(&copy_snapshot_intent(&progress))
            .expect("refreshed cleanup snapshot");
        let mut stale_cleanup_generation =
            serde_json::to_value(&refreshed_snapshot).expect("snapshot");
        stale_cleanup_generation["cancellation_cleanup_observations"]["recovery_generation"] =
            json!(3);
        let stale_cleanup_generation: FullProgressSnapshotObservation =
            serde_json::from_value(stale_cleanup_generation)
                .expect("stale cleanup generation shape");
        assert!(matches!(
            stale_cleanup_generation.validate_base(),
            Err(
                FullProgressSnapshotObservationError::CancellationCleanupGenerationNotBeforeCurrentAfterEffect {
                    cleanup: 3,
                    after_effect: 3,
                }
            )
        ));
    }

    #[test]
    fn full_progress_snapshot_validates_after_effect_refinement_from_prepublish_dispatch() {
        let mut progress = copy_progress_with_admission_claims();
        progress
            .request_cancellation(CancellationRequestObservation::new(18109).expect("cancel"))
            .expect("cancellation request");
        let manifest = progress
            .cancellation_dispatch_manifest()
            .expect("dispatch manifest");
        let mut prepublish = pre_publish_cleanup_authority(
            &progress,
            PrePublishCleanupObservation::new(Vec::new()).expect("cleanup"),
            settlement_inventory(&progress, AdmissionClaimSettlementOutcome::Held),
        );
        prepublish.dispatch_outcome = CancellationDispatchOutcomeRecord::new(
            manifest.clone(),
            progress.execution_epoch(),
            1,
            dispatch_rows(&manifest, None),
        )
        .expect("uncertain prepublish dispatch");
        progress
            .finish_pre_publish_cancellation(prepublish)
            .expect("retain uncertain prepublish dispatch");

        let dispatch = all_never_dispatch_record(&progress);
        let (recovery, source, coordinator) = empty_after_effect_inputs(&progress, dispatch);
        progress
            .accept_after_effect_reconciliation(after_effect_authority(
                &progress,
                recovery,
                source,
                coordinator,
                progress.recovery_generation(),
            ))
            .expect("refine prepublish dispatch into after-effect state");
        let snapshot = progress
            .capture_full_progress_snapshot(&copy_snapshot_intent(&progress))
            .expect("refined after-effect snapshot");
        assert_eq!(
            snapshot
                .after_effect_reconciliation()
                .expect("after-effect state")
                .recovery_generation(),
            2
        );
        snapshot
            .validate_base()
            .expect("prepublish dispatch refinement validates");
    }

    #[test]
    fn full_progress_snapshot_validates_cancellation_envelopes_and_routes() {
        let mut before_publish = copy_progress_with_admission_claims();
        before_publish
            .request_cancellation(CancellationRequestObservation::new(18111).expect("cancel"))
            .expect("before-publish cancellation");
        before_publish
            .capture_full_progress_snapshot(&copy_snapshot_intent(&before_publish))
            .expect("before-publish cancellation snapshot")
            .validate_base()
            .expect("before-publish cancellation envelope");

        let mut after_publish = copy_progress_with_admission_claims();
        publish_prefix(&mut after_publish);
        after_publish
            .request_cancellation(CancellationRequestObservation::new(18112).expect("cancel"))
            .expect("after-publish cancellation");
        after_publish
            .capture_full_progress_snapshot(&copy_snapshot_intent(&after_publish))
            .expect("after-publish cancellation snapshot")
            .validate_base()
            .expect("after-publish cancellation envelope");

        let (participants, source, _filesystem, source_id) = valid_topology(false);
        let args = retained_resolution_args(source, source_id, false);
        let mut after_effect = claimed_progress(participants.clone(), args.clone(), 18113);
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::Prepare,
            WorkUnitRole::SelectedRetainedObjectDisposition,
        ] {
            apply_role(&mut after_effect, 0, role);
        }
        after_effect
            .request_cancellation(CancellationRequestObservation::new(18113).expect("cancel"))
            .expect("after-observed-effect cancellation");
        after_effect
            .capture_full_progress_snapshot(&snapshot_intent_for(&after_effect, args, participants))
            .expect("after-observed-effect cancellation snapshot")
            .validate_base()
            .expect("after-observed-effect cancellation envelope");

        let mut audited = copy_progress_with_admission_claims();
        publish_prefix(&mut audited);
        audited
            .request_cancellation(CancellationRequestObservation::new(18114).expect("cancel"))
            .expect("audited cancellation");
        audited
            .require_audit(ProgressAuditObservation::new(
                ProgressAuditReason::LateCollision,
            ))
            .expect("audit over cancellation");
        audited
            .capture_full_progress_snapshot(&copy_snapshot_intent(&audited))
            .expect("audited cancellation snapshot")
            .validate_base()
            .expect("audit cancellation hold matches route");

        let mut retry = copy_progress_with_admission_claims();
        retry
            .request_retry(
                RetryObservation::new(
                    1,
                    0,
                    1,
                    RetryLeaseId::new("snapshot-cancel-retry").expect("retry lease"),
                    10,
                    RetryErrorClass::TransientContention,
                    "snapshot cancellation retry",
                    None,
                )
                .expect("retry observation"),
            )
            .expect("retry request");
        retry
            .request_cancellation(CancellationRequestObservation::new(18115).expect("cancel"))
            .expect("cancellation while retry remains active");
        retry
            .capture_full_progress_snapshot(&copy_snapshot_intent(&retry))
            .expect("retry cancellation snapshot")
            .validate_base()
            .expect("retry and cancellation envelope");

        let mut reacquired = copy_progress_with_admission_claims();
        reacquired
            .request_cancellation(CancellationRequestObservation::new(18116).expect("cancel"))
            .expect("cancellation before reacquisition");
        let source_key = key_for(&reacquired, 0, WorkUnitRole::SourceRead);
        reacquired
            .reacquire_source(source_reacquisition_authority(
                &reacquired,
                source_key.participant_id(),
                7,
                2,
                0,
            ))
            .expect("reacquire after cancellation");
        let retained = reacquired
            .capture_full_progress_snapshot(&copy_snapshot_intent(&reacquired))
            .expect("historical cancellation snapshot");
        assert_eq!(
            retained
                .cancellation_inventory()
                .expect("retained inventory")
                .execution_epoch(),
            0
        );
        assert_eq!(reacquired.execution_epoch(), 1);
        retained
            .validate_base()
            .expect("historical cancellation inventory remains valid");

        let mut terminal = copy_progress_with_admission_claims();
        terminal
            .request_cancellation(CancellationRequestObservation::new(18117).expect("cancel"))
            .expect("terminal cancellation request");
        terminal
            .finish_pre_publish_cancellation(pre_publish_cleanup_authority(
                &terminal,
                PrePublishCleanupObservation::new(Vec::new()).expect("empty cleanup"),
                settlement_inventory(&terminal, AdmissionClaimSettlementOutcome::Released),
            ))
            .expect("pre-publish terminal cancellation");
        assert_eq!(
            terminal.disposition(),
            FullProgressDisposition::CancelledBeforePublish
        );
        terminal
            .capture_full_progress_snapshot(&copy_snapshot_intent(&terminal))
            .expect("terminal cancellation snapshot")
            .validate_base()
            .expect("terminal cancellation envelope");
    }

    #[test]
    fn full_progress_snapshot_validates_retained_pre_publish_proof_and_corruption() {
        let mut partial = copy_progress_with_admission_claims();
        apply_role(&mut partial, 0, WorkUnitRole::SourceRead);
        apply_role(&mut partial, 0, WorkUnitRole::Prepare);
        partial
            .request_cancellation(CancellationRequestObservation::new(18127).expect("cancel"))
            .expect("partial cancellation");
        let partial_cleanup =
            PrePublishCleanupObservation::new(vec![PrePublishActionCleanup::new(
                0,
                Some(PrePublishFinalClaimObservation::NoFinalClaim),
                Some(PrePublishOriginalStateObservation::Preserved),
                Some(PrePublishAbsenceObservation::Preserved),
                None,
            )])
            .expect("partial cleanup");
        partial
            .finish_pre_publish_cancellation(pre_publish_cleanup_authority(
                &partial,
                partial_cleanup,
                settlement_inventory(&partial, AdmissionClaimSettlementOutcome::Held),
            ))
            .expect("retain incomplete prepublication proof");
        let partial_snapshot = partial
            .capture_full_progress_snapshot(&copy_snapshot_intent(&partial))
            .expect("partial prepublication snapshot");
        partial_snapshot
            .validate_base()
            .expect("incomplete held prepublication proof validates");

        let mut wrong_origin = serde_json::to_value(&partial_snapshot).expect("snapshot value");
        wrong_origin["pre_publish_cancellation"]["cleanup"]["actions"][0]["original_state"] =
            json!("NoOriginalObject");
        let wrong_origin: FullProgressSnapshotObservation =
            serde_json::from_value(wrong_origin).expect("wrong origin shape");
        assert!(matches!(
            wrong_origin.validate_base(),
            Err(
                FullProgressSnapshotObservationError::CancellationPrePublishValidation {
                    source: WorkProgressError::PrePublishCleanupOriginalStateMismatch {
                        action_ordinal: 0,
                    },
                }
            )
        ));

        let mut uncertain = copy_progress_with_two_admission_claims();
        apply_role(&mut uncertain, 0, WorkUnitRole::SourceRead);
        uncertain
            .request_cancellation(CancellationRequestObservation::new(18128).expect("cancel"))
            .expect("uncertain cancellation");
        let quiescence = unstarted_quiescence_for_action(
            &uncertain,
            0,
            PrePublishUnstartedQuiescenceResult::Uncertain,
            Some(PrePublishAbsenceObservation::Preserved),
            Some(PrePublishAbsenceObservation::Preserved),
        );
        uncertain
            .finish_pre_publish_cancellation(pre_publish_cleanup_authority_with_quiescence(
                &uncertain,
                quiescence,
                PrePublishCleanupObservation::new(Vec::new()).expect("empty cleanup"),
                settlement_inventory(&uncertain, AdmissionClaimSettlementOutcome::Held),
            ))
            .expect("retain uncertain quiescence");
        let uncertain_snapshot = uncertain
            .capture_full_progress_snapshot(&copy_snapshot_intent_with_capacity(
                &uncertain,
                two_claim_capacity_plan(),
            ))
            .expect("uncertain quiescence snapshot");
        uncertain_snapshot
            .validate_base()
            .expect("uncertain quiescence validates");
        let mut wrong_quiescence =
            serde_json::to_value(&uncertain_snapshot).expect("snapshot value");
        wrong_quiescence["pre_publish_cancellation"]["unstarted_quiescence"]["actions"][0]["eligible_pending_keys"] =
            json!([]);
        let wrong_quiescence: FullProgressSnapshotObservation =
            serde_json::from_value(wrong_quiescence).expect("wrong quiescence shape");
        assert!(matches!(
            wrong_quiescence.validate_base(),
            Err(
                FullProgressSnapshotObservationError::CancellationPrePublishValidation {
                    source: WorkProgressError::PrePublishUnstartedQuiescenceKeySetMismatch { .. },
                }
            )
        ));

        let mut historical = copy_progress_with_admission_claims();
        historical
            .request_cancellation(CancellationRequestObservation::new(18129).expect("cancel"))
            .expect("historical cancellation");
        historical
            .finish_pre_publish_cancellation(pre_publish_cleanup_authority(
                &historical,
                PrePublishCleanupObservation::new(Vec::new()).expect("empty cleanup"),
                settlement_inventory(&historical, AdmissionClaimSettlementOutcome::Held),
            ))
            .expect("retain historical prepublication proof");
        let source_key = key_for(&historical, 0, WorkUnitRole::SourceRead);
        historical
            .reacquire_source(source_reacquisition_authority(
                &historical,
                source_key.participant_id(),
                7,
                2,
                historical.execution_epoch(),
            ))
            .expect("source reacquisition");
        let historical_snapshot = historical
            .capture_full_progress_snapshot(&copy_snapshot_intent(&historical))
            .expect("historical prepublication snapshot");
        historical_snapshot
            .validate_base()
            .expect("historical dispatch remains valid after reacquisition");
        assert_eq!(
            historical_snapshot
                .pre_publish_cancellation()
                .expect("historical prepublication")
                .dispatch_outcome()
                .expect("historical dispatch")
                .observation_epoch(),
            0
        );

        let mut terminal = copy_progress_with_admission_claims();
        terminal
            .request_cancellation(CancellationRequestObservation::new(18130).expect("cancel"))
            .expect("terminal cancellation");
        terminal
            .finish_pre_publish_cancellation(pre_publish_cleanup_authority(
                &terminal,
                PrePublishCleanupObservation::new(Vec::new()).expect("empty cleanup"),
                settlement_inventory(&terminal, AdmissionClaimSettlementOutcome::Released),
            ))
            .expect("terminal cancellation");
        let terminal_snapshot = terminal
            .capture_full_progress_snapshot(&copy_snapshot_intent(&terminal))
            .expect("terminal snapshot");
        terminal_snapshot
            .validate_base()
            .expect("complete terminal proof validates");

        let mut missing_dispatch =
            serde_json::to_value(&terminal_snapshot).expect("snapshot value");
        missing_dispatch["pre_publish_cancellation"]["dispatch_outcome"] = serde_json::Value::Null;
        let missing_dispatch: FullProgressSnapshotObservation =
            serde_json::from_value(missing_dispatch).expect("missing dispatch shape");
        assert!(matches!(
            missing_dispatch.validate_base(),
            Err(FullProgressSnapshotObservationError::CancellationPrePublishDispatchMissing)
        ));

        let mut dispatch_future = serde_json::to_value(&terminal_snapshot).expect("snapshot value");
        dispatch_future["pre_publish_cancellation"]["dispatch_outcome"]["recovery_generation"] =
            json!(2);
        let dispatch_future: FullProgressSnapshotObservation =
            serde_json::from_value(dispatch_future).expect("future dispatch shape");
        assert!(matches!(
            dispatch_future.validate_base(),
            Err(
                FullProgressSnapshotObservationError::CancellationPrePublishDispatchGenerationFuture {
                    dispatch: 2,
                    current: 1,
                }
            )
        ));

        let mut incomplete_terminal =
            serde_json::to_value(&terminal_snapshot).expect("snapshot value");
        assert!(
            incomplete_terminal["pre_publish_cancellation"]["dispatch_outcome"]["rows"]
                .as_array()
                .is_some_and(|rows| !rows.is_empty())
        );
        incomplete_terminal["pre_publish_cancellation"]["dispatch_outcome"]["rows"][0]["outcome"] =
            json!("EntryUncertain");
        let incomplete_terminal: FullProgressSnapshotObservation =
            serde_json::from_value(incomplete_terminal).expect("incomplete terminal shape");
        assert!(
            matches!(
                incomplete_terminal.validate_base(),
                Err(
                    FullProgressSnapshotObservationError::CancellationPrePublishValidation {
                        source: WorkProgressError::PrePublishCancellationDispatchIncomplete,
                    }
                )
            ),
            "incomplete dispatch must be rejected before terminal proof"
        );

        let mut unsettled_terminal =
            serde_json::to_value(&terminal_snapshot).expect("snapshot value");
        unsettled_terminal["pre_publish_cancellation"]["settlement"]["settlements"][0]["outcome"] =
            json!("Held");
        let unsettled_terminal: FullProgressSnapshotObservation =
            serde_json::from_value(unsettled_terminal).expect("unsettled terminal shape");
        assert!(matches!(
            unsettled_terminal.validate_base(),
            Err(
                FullProgressSnapshotObservationError::CancellationPrePublishTerminalProofIncomplete
            )
        ));

        let mut incompatible_settlement =
            serde_json::to_value(&terminal_snapshot).expect("snapshot value");
        incompatible_settlement["pre_publish_cancellation"]["settlement"]["settlements"][0]["outcome"] =
            json!("ConsumedOrTransferred");
        let incompatible_settlement: FullProgressSnapshotObservation =
            serde_json::from_value(incompatible_settlement).expect("incompatible settlement shape");
        assert!(matches!(
            incompatible_settlement.validate_base(),
            Err(
                FullProgressSnapshotObservationError::CancellationPrePublishValidation {
                    source: WorkProgressError::PrePublishCancellationSettlementClassMismatch {
                        allocation_class: FullCapacityAllocationClass::DestinationStaging,
                        outcome: AdmissionClaimSettlementOutcome::ConsumedOrTransferred,
                        ..
                    },
                }
            )
        ));

        let mut early_nonheld = serde_json::to_value(&uncertain_snapshot).expect("snapshot value");
        early_nonheld["pre_publish_cancellation"]["settlement"]["settlements"][0]["outcome"] =
            json!("Released");
        let early_nonheld: FullProgressSnapshotObservation =
            serde_json::from_value(early_nonheld).expect("early non-held settlement shape");
        assert!(matches!(
            early_nonheld.validate_base(),
            Err(
                FullProgressSnapshotObservationError::CancellationPrePublishValidation {
                    source: WorkProgressError::PrePublishCancellationSettlementGateIncomplete,
                }
            )
        ));

        let mut wrong_terminal_phase =
            serde_json::to_value(&terminal_snapshot).expect("snapshot value");
        wrong_terminal_phase["phase"] = json!("Prepared");
        let wrong_terminal_phase: FullProgressSnapshotObservation =
            serde_json::from_value(wrong_terminal_phase).expect("wrong terminal phase shape");
        assert!(matches!(
            wrong_terminal_phase.validate_base(),
            Err(
                FullProgressSnapshotObservationError::CancellationPrePublishTerminalStateMismatch {
                    phase: OperationPhase::Prepared,
                    disposition: FullProgressDisposition::CancelledBeforePublish,
                }
            )
        ));

        let mut wrong_terminal_disposition =
            serde_json::to_value(&terminal_snapshot).expect("snapshot value");
        wrong_terminal_disposition["disposition"] = json!("CancelRequestedBeforePublish");
        let wrong_terminal_disposition: FullProgressSnapshotObservation =
            serde_json::from_value(wrong_terminal_disposition)
                .expect("wrong terminal disposition shape");
        assert!(matches!(
            wrong_terminal_disposition.validate_base(),
            Err(
                FullProgressSnapshotObservationError::CancellationPrePublishTerminalStateMismatch {
                    phase: OperationPhase::Terminal,
                    disposition: FullProgressDisposition::CancelRequestedBeforePublish,
                }
            )
        ));
    }

    #[test]
    fn full_progress_snapshot_requires_complete_held_settlement_and_retains_it() {
        let incomplete = copy_progress_with_admission_claims();
        let incomplete_snapshot = incomplete
            .capture_full_progress_snapshot(&copy_snapshot_intent(&incomplete))
            .expect("incomplete snapshot");
        let mut premature_held = serde_json::to_value(&incomplete_snapshot).expect("snapshot");
        premature_held["claim_settlement"] = serde_json::to_value(settlement_inventory(
            &incomplete,
            AdmissionClaimSettlementOutcome::Held,
        ))
        .expect("held settlement");
        let premature_held: FullProgressSnapshotObservation =
            serde_json::from_value(premature_held).expect("premature held snapshot");
        assert!(matches!(
            premature_held.validate_base(),
            Err(FullProgressSnapshotObservationError::ClaimSettlementProgressIncomplete)
        ));

        let mut retained = copy_progress_with_admission_claims();
        complete_required(&mut retained);
        retained
            .apply_verified_claim_settlement(settlement_authority(
                &retained,
                settlement_inventory(&retained, AdmissionClaimSettlementOutcome::Held),
            ))
            .expect("held settlement applies after complete progress");
        retained
            .capture_full_progress_snapshot(&copy_snapshot_intent(&retained))
            .expect("held settlement snapshot")
            .validate_base()
            .expect("held settlement remains valid");

        let mut held_audit = copy_progress_with_admission_claims();
        complete_required(&mut held_audit);
        held_audit
            .apply_verified_claim_settlement(settlement_authority(
                &held_audit,
                settlement_inventory(&held_audit, AdmissionClaimSettlementOutcome::Held),
            ))
            .expect("held settlement for audit");
        held_audit
            .require_audit(ProgressAuditObservation::new(
                ProgressAuditReason::RevisionGap,
            ))
            .expect("audit retains held settlement");
        held_audit
            .capture_full_progress_snapshot(&copy_snapshot_intent(&held_audit))
            .expect("held audit snapshot")
            .validate_base()
            .expect("held settlement remains valid under audit");

        let retry_observation = RetryObservation::new(
            1,
            100,
            200,
            RetryLeaseId::new("snapshot-held-transition").expect("retry lease"),
            300,
            RetryErrorClass::TransientAvailability,
            "held settlement retry",
            None,
        )
        .expect("retry observation");
        retained
            .request_retry(retry_observation.clone())
            .expect("retry with held settlement");
        retained
            .capture_full_progress_snapshot(&copy_snapshot_intent(&retained))
            .expect("held retry snapshot")
            .validate_base()
            .expect("held settlement remains valid during retry");
        retained
            .resume_retry(VerifiedRetryResume {
                operation_id: retained.operation_id,
                plan: retained.plan.clone(),
                retry_observation,
                now_unix_ms: 200,
                expected_execution_epoch: 0,
            })
            .expect("resume held settlement retry");
        retained
            .request_cancellation(
                CancellationRequestObservation::new(18131).expect("cancel held settlement"),
            )
            .expect("cancel after held settlement retry");
        let source_key = key_for(&retained, 0, WorkUnitRole::SourceRead);
        retained
            .reacquire_source(source_reacquisition_authority(
                &retained,
                source_key.participant_id(),
                7,
                2,
                retained.execution_epoch(),
            ))
            .expect("reacquire after held settlement cancellation");
        retained
            .capture_full_progress_snapshot(&copy_snapshot_intent(&retained))
            .expect("held settlement historical snapshot")
            .validate_base()
            .expect("held settlement remains valid after cancellation and reacquisition");
    }

    #[test]
    fn full_progress_snapshot_validates_success_and_deferred_settlement_lifecycle() {
        let mut success = copy_progress_with_admission_claims();
        complete_required(&mut success);
        success
            .apply_verified_claim_settlement(settlement_authority(
                &success,
                settlement_inventory(&success, AdmissionClaimSettlementOutcome::Released),
            ))
            .expect("normal success settlement");
        let success_snapshot = success
            .capture_full_progress_snapshot(&copy_snapshot_intent(&success))
            .expect("normal success snapshot");
        assert_eq!(
            success_snapshot.disposition(),
            FullProgressDisposition::Succeeded
        );
        assert_eq!(success_snapshot.phase(), OperationPhase::Terminal);
        success_snapshot
            .validate_base()
            .expect("normal success lifecycle");

        let mut missing_settlement = serde_json::to_value(&success_snapshot).expect("snapshot");
        missing_settlement["claim_settlement"] = serde_json::Value::Null;
        let missing_settlement: FullProgressSnapshotObservation =
            serde_json::from_value(missing_settlement).expect("missing settlement shape");
        assert!(matches!(
            missing_settlement.validate_base(),
            Err(
                FullProgressSnapshotObservationError::ClaimSettlementMissingForSuccess {
                    disposition: FullProgressDisposition::Succeeded,
                }
            )
        ));

        let mut held_settlement = serde_json::to_value(&success_snapshot).expect("snapshot");
        held_settlement["claim_settlement"] = serde_json::to_value(settlement_inventory(
            &success,
            AdmissionClaimSettlementOutcome::Held,
        ))
        .expect("held settlement");
        let held_settlement: FullProgressSnapshotObservation =
            serde_json::from_value(held_settlement).expect("held settlement shape");
        assert!(matches!(
            held_settlement.validate_base(),
            Err(FullProgressSnapshotObservationError::ClaimSettlementIncompleteForSuccess)
        ));

        let mut unknown_settlement_claim =
            serde_json::to_value(&success_snapshot).expect("snapshot");
        unknown_settlement_claim["claim_settlement"]["settlements"][0]["claim_id"] =
            json!(Uuid::from_u128(299).to_string());
        let unknown_settlement_claim: FullProgressSnapshotObservation =
            serde_json::from_value(unknown_settlement_claim).expect("unknown claim shape");
        assert!(matches!(
            unknown_settlement_claim.validate_base(),
            Err(FullProgressSnapshotObservationError::ClaimSettlement(
                AdmissionClaimValidationError::UnknownSettlementClaim { .. }
            ))
        ));

        let mut retained_success =
            copy_progress_with_claim_class(FullCapacityAllocationClass::JournalRecord, 2121);
        complete_required(&mut retained_success);
        retained_success
            .apply_verified_claim_settlement(settlement_authority(
                &retained_success,
                settlement_inventory(
                    &retained_success,
                    AdmissionClaimSettlementOutcome::ConsumedOrTransferred,
                ),
            ))
            .expect("retained-charge success settlement");
        let retained_capacity = FullCapacityPlan::new(vec![
            FullCapacityAllocation::new(
                VolumeIdentity { device: 1 },
                1,
                FullCapacityAllocationClass::JournalRecord,
                12,
                12,
                PROTECTED_FREE_SPACE_FLOOR,
            )
            .expect("retained capacity allocation"),
        ])
        .expect("retained capacity plan");
        let retained_success_snapshot = retained_success
            .capture_full_progress_snapshot(&copy_snapshot_intent_with_capacity(
                &retained_success,
                retained_capacity,
            ))
            .expect("retained-charge success snapshot");
        let mut released_retained_charge =
            serde_json::to_value(&retained_success_snapshot).expect("snapshot");
        released_retained_charge["claim_settlement"]["settlements"][0]["outcome"] =
            json!("Released");
        let released_retained_charge: FullProgressSnapshotObservation =
            serde_json::from_value(released_retained_charge)
                .expect("released retained-charge shape");
        assert!(matches!(
            released_retained_charge.validate_base(),
            Err(FullProgressSnapshotObservationError::ClaimSettlement(
                AdmissionClaimValidationError::NormalSuccessRetainedChargeReleased {
                    allocation_class: FullCapacityAllocationClass::JournalRecord,
                    ..
                }
            ))
        ));

        let mut deferred = copy_progress_with_admission_claims_and_optional();
        complete_required(&mut deferred);
        let artifact_key = deferred
            .steps()
            .iter()
            .find(|step| step.requirement() == ParticipantRequirement::OptionalRebuildable)
            .map(WorkProgressStep::key)
            .expect("optional artifact");
        apply_readiness_deferral(&mut deferred, &[artifact_key], 1);
        deferred
            .apply_verified_claim_settlement(settlement_authority(
                &deferred,
                settlement_inventory(&deferred, AdmissionClaimSettlementOutcome::Released),
            ))
            .expect("deferred success settlement");
        let (participants, source, _filesystem, source_id) = valid_topology(true);
        let deferred_snapshot = deferred
            .capture_full_progress_snapshot(&snapshot_intent_for(
                &deferred,
                copy_args_at(source, source_id, "copy.wav"),
                participants,
            ))
            .expect("deferred success snapshot");
        assert_eq!(
            deferred_snapshot.disposition(),
            FullProgressDisposition::SucceededWithDeferredArtifacts
        );
        assert_eq!(deferred_snapshot.phase(), OperationPhase::Terminal);
        deferred_snapshot
            .validate_base()
            .expect("deferred success lifecycle");
    }

    #[test]
    fn full_progress_snapshot_rejects_lifecycle_phase_retry_audit_and_generation_corruption() {
        let initial = copy_progress_with_admission_claims();
        let initial_snapshot = initial
            .capture_full_progress_snapshot(&copy_snapshot_intent(&initial))
            .expect("initial snapshot");
        initial_snapshot
            .validate_base()
            .expect("initial no-completed lifecycle");

        let mut phase_ahead = serde_json::to_value(&initial_snapshot).expect("snapshot");
        phase_ahead["phase"] = json!("ReadinessScheduled");
        let phase_ahead: FullProgressSnapshotObservation =
            serde_json::from_value(phase_ahead).expect("phase-ahead shape");
        assert!(matches!(
            phase_ahead.validate_base(),
            Err(FullProgressSnapshotObservationError::DerivedPhaseMismatch {
                expected: OperationPhase::IntentDurable,
                actual: OperationPhase::ReadinessScheduled,
            })
        ));

        let mut completed = copy_progress_with_admission_claims();
        complete_required(&mut completed);
        let completed_snapshot = completed
            .capture_full_progress_snapshot(&copy_snapshot_intent(&completed))
            .expect("completed unsolved snapshot");
        completed_snapshot
            .validate_base()
            .expect("completed unsolved lifecycle");
        let mut phase_behind = serde_json::to_value(&completed_snapshot).expect("snapshot");
        phase_behind["phase"] = json!("IntentDurable");
        let phase_behind: FullProgressSnapshotObservation =
            serde_json::from_value(phase_behind).expect("phase-behind shape");
        assert!(matches!(
            phase_behind.validate_base(),
            Err(FullProgressSnapshotObservationError::DerivedPhaseMismatch {
                expected: OperationPhase::ReadinessScheduled,
                actual: OperationPhase::IntentDurable,
            })
        ));

        let mut unsupported = serde_json::to_value(&initial_snapshot).expect("snapshot");
        unsupported["disposition"] = json!("PartialNeedsRetry");
        let unsupported: FullProgressSnapshotObservation =
            serde_json::from_value(unsupported).expect("unsupported disposition shape");
        assert!(matches!(
            unsupported.validate_base(),
            Err(
                FullProgressSnapshotObservationError::UnsupportedDisposition {
                    disposition: FullProgressDisposition::PartialNeedsRetry,
                }
            )
        ));

        let mut retry = copy_progress_with_admission_claims();
        let retry_observation = RetryObservation::new(
            1,
            100,
            200,
            RetryLeaseId::new("snapshot-lifecycle-retry").expect("retry lease"),
            300,
            RetryErrorClass::TransientAvailability,
            "snapshot lifecycle retry",
            None,
        )
        .expect("retry observation");
        retry
            .request_retry(retry_observation.clone())
            .expect("request retry");
        let retry_snapshot = retry
            .capture_full_progress_snapshot(&copy_snapshot_intent(&retry))
            .expect("retry snapshot");
        let mut retry_under_none = serde_json::to_value(&retry_snapshot).expect("snapshot");
        retry_under_none["disposition"] = json!("None");
        let retry_under_none: FullProgressSnapshotObservation =
            serde_json::from_value(retry_under_none).expect("retry-under-none shape");
        assert!(matches!(
            retry_under_none.validate_base(),
            Err(
                FullProgressSnapshotObservationError::RetryDispositionMismatch {
                    disposition: FullProgressDisposition::None,
                }
            )
        ));

        let mut published_audit = copy_progress_with_admission_claims();
        publish_prefix(&mut published_audit);
        published_audit
            .require_audit(ProgressAuditObservation::new(
                ProgressAuditReason::RevisionGap,
            ))
            .expect("published audit");
        let published_audit_snapshot = published_audit
            .capture_full_progress_snapshot(&copy_snapshot_intent(&published_audit))
            .expect("published audit snapshot");
        let mut audit_retry_none =
            serde_json::to_value(&published_audit_snapshot).expect("snapshot");
        audit_retry_none["retry_observation"] =
            serde_json::to_value(&retry_observation).expect("retry observation");
        audit_retry_none["last_retry_observation"] =
            serde_json::to_value(&retry_observation).expect("retry observation");
        let audit_retry_none: FullProgressSnapshotObservation =
            serde_json::from_value(audit_retry_none).expect("audit hold shape");
        assert!(matches!(
            audit_retry_none.validate_base(),
            Err(
                FullProgressSnapshotObservationError::RetryDispositionMismatch {
                    disposition: FullProgressDisposition::AuditRequired,
                }
            )
        ));

        let mut audit_before_publish = serde_json::to_value(&initial_snapshot).expect("snapshot");
        let published_audit_value =
            serde_json::to_value(&published_audit_snapshot).expect("snapshot");
        audit_before_publish["disposition"] = json!("AuditRequired");
        audit_before_publish["active_audit"] = published_audit_value["active_audit"].clone();
        audit_before_publish["audit_generation"] = json!(1);
        let audit_before_publish: FullProgressSnapshotObservation =
            serde_json::from_value(audit_before_publish).expect("prepublication audit shape");
        assert!(matches!(
            audit_before_publish.validate_base(),
            Err(FullProgressSnapshotObservationError::AuditPhaseInvalid {
                phase: OperationPhase::IntentDurable,
            })
        ));

        let mut fabricated_generation = serde_json::to_value(&initial_snapshot).expect("snapshot");
        fabricated_generation["recovery_generation"] = json!(1);
        let fabricated_generation: FullProgressSnapshotObservation =
            serde_json::from_value(fabricated_generation).expect("fabricated generation shape");
        assert!(matches!(
            fabricated_generation.validate_base(),
            Err(
                FullProgressSnapshotObservationError::RecoveryGenerationMismatch {
                    expected: 0,
                    actual: 1,
                }
            )
        ));

        let mut accepted_cleanup = copy_progress_with_admission_claims();
        accepted_cleanup
            .request_cancellation(CancellationRequestObservation::new(18132).expect("cancel"))
            .expect("cancellation request");
        accept_empty_after_effect_reconciliation(&mut accepted_cleanup);
        accept_pending_cleanup_observations(&mut accepted_cleanup);
        let accepted_cleanup_snapshot = accepted_cleanup
            .capture_full_progress_snapshot(&copy_snapshot_intent(&accepted_cleanup))
            .expect("after-effect and cleanup snapshot");
        let mut wrong_global_generation =
            serde_json::to_value(&accepted_cleanup_snapshot).expect("snapshot");
        wrong_global_generation["recovery_generation"] = json!(3);
        let wrong_global_generation: FullProgressSnapshotObservation =
            serde_json::from_value(wrong_global_generation).expect("wrong global generation shape");
        assert!(matches!(
            wrong_global_generation.validate_base(),
            Err(
                FullProgressSnapshotObservationError::RecoveryGenerationMismatch {
                    expected: 2,
                    actual: 3,
                }
            )
        ));
    }

    #[test]
    fn full_progress_snapshot_rejects_cancellation_envelope_corruption() {
        let mut progress = copy_progress_with_admission_claims();
        progress
            .request_cancellation(CancellationRequestObservation::new(18121).expect("cancel"))
            .expect("cancellation");
        let snapshot = progress
            .capture_full_progress_snapshot(&copy_snapshot_intent(&progress))
            .expect("cancellation snapshot");

        let mut one_sided = serde_json::to_value(&snapshot).expect("snapshot value");
        one_sided["cancellation_inventory"] = serde_json::Value::Null;
        let one_sided: FullProgressSnapshotObservation =
            serde_json::from_value(one_sided).expect("one-sided envelope shape");
        assert!(matches!(
            one_sided.validate_base(),
            Err(
                FullProgressSnapshotObservationError::CancellationEnvelopePairMismatch {
                    request_present: true,
                    inventory_present: false,
                }
            )
        ));

        let mut negative_request = serde_json::to_value(&snapshot).expect("snapshot value");
        negative_request["cancellation_request"]["requested_unix_ms"] = json!(-1);
        let negative_request: FullProgressSnapshotObservation =
            serde_json::from_value(negative_request).expect("negative request shape");
        assert!(matches!(
            negative_request.validate_base(),
            Err(FullProgressSnapshotObservationError::CancellationRequest(
                CancellationRequestValidationError::NegativeTimestamp
            ))
        ));

        let mut wrong_operation = serde_json::to_value(&snapshot).expect("snapshot value");
        wrong_operation["cancellation_inventory"]["operation_id"] =
            json!(Uuid::from_u128(18122).to_string());
        let wrong_operation: FullProgressSnapshotObservation =
            serde_json::from_value(wrong_operation).expect("wrong operation shape");
        assert!(matches!(
            wrong_operation.validate_base(),
            Err(FullProgressSnapshotObservationError::CancellationInventory(
                CancellationInventoryError::OperationIdMismatch
            ))
        ));

        let mut wrong_plan = serde_json::to_value(&snapshot).expect("snapshot value");
        wrong_plan["cancellation_inventory"]["plan_version"] =
            json!(snapshot.intent().plan().plan_version() + 1);
        let wrong_plan: FullProgressSnapshotObservation =
            serde_json::from_value(wrong_plan).expect("wrong plan shape");
        assert!(matches!(
            wrong_plan.validate_base(),
            Err(FullProgressSnapshotObservationError::CancellationInventory(
                CancellationInventoryError::PlanVersionMismatch { .. }
            ))
        ));

        let mut wrong_claims = serde_json::to_value(&snapshot).expect("snapshot value");
        wrong_claims["cancellation_inventory"]["admission_claims"]["claims"][0]["allocated_bytes"] =
            json!(11);
        let wrong_claims: FullProgressSnapshotObservation =
            serde_json::from_value(wrong_claims).expect("wrong claims shape");
        assert!(matches!(
            wrong_claims.validate_base(),
            Err(FullProgressSnapshotObservationError::CancellationInventory(
                CancellationInventoryError::AdmissionClaimsMismatch
            ))
        ));

        let mut wrong_step = serde_json::to_value(&snapshot).expect("snapshot value");
        wrong_step["cancellation_inventory"]["steps"][0]["key"]["step_ordinal"] = json!(999);
        let wrong_step: FullProgressSnapshotObservation =
            serde_json::from_value(wrong_step).expect("wrong step shape");
        assert!(matches!(
            wrong_step.validate_base(),
            Err(FullProgressSnapshotObservationError::CancellationInventory(
                CancellationInventoryError::StepKeyMismatch { .. }
            ))
        ));

        let mut wrong_action = serde_json::to_value(&snapshot).expect("snapshot value");
        wrong_action["cancellation_inventory"]["actions"][0]["state"] = json!("AppPublished");
        let wrong_action: FullProgressSnapshotObservation =
            serde_json::from_value(wrong_action).expect("wrong action shape");
        assert!(matches!(
            wrong_action.validate_base(),
            Err(FullProgressSnapshotObservationError::CancellationInventory(
                CancellationInventoryError::ActionStateMismatch { .. }
            ))
        ));

        let mut wrong_route = serde_json::to_value(&snapshot).expect("snapshot value");
        wrong_route["cancellation_inventory"]["route"] = json!("AfterPublish");
        let wrong_route: FullProgressSnapshotObservation =
            serde_json::from_value(wrong_route).expect("wrong route shape");
        assert!(matches!(
            wrong_route.validate_base(),
            Err(FullProgressSnapshotObservationError::CancellationInventory(
                CancellationInventoryError::RouteMismatch { .. }
            ))
        ));

        let mut future_epoch = serde_json::to_value(&snapshot).expect("snapshot value");
        future_epoch["cancellation_inventory"]["execution_epoch"] = json!(1);
        let future_epoch: FullProgressSnapshotObservation =
            serde_json::from_value(future_epoch).expect("future epoch shape");
        assert!(matches!(
            future_epoch.validate_base(),
            Err(FullProgressSnapshotObservationError::CancellationInventory(
                CancellationInventoryError::ExecutionEpochFuture { .. }
            ))
        ));

        let mut wrong_disposition = serde_json::to_value(&snapshot).expect("snapshot value");
        wrong_disposition["disposition"] = json!("CancelRequestedAfterPublish");
        let wrong_disposition: FullProgressSnapshotObservation =
            serde_json::from_value(wrong_disposition).expect("wrong disposition shape");
        assert!(matches!(
            wrong_disposition.validate_base(),
            Err(
                FullProgressSnapshotObservationError::CancellationDispositionMismatch {
                    route: CancellationRoute::BeforePublish,
                    disposition: FullProgressDisposition::CancelRequestedAfterPublish,
                }
            )
        ));

        let mut audited = copy_progress_with_admission_claims();
        publish_prefix(&mut audited);
        audited
            .request_cancellation(CancellationRequestObservation::new(18123).expect("cancel"))
            .expect("audited cancellation");
        audited
            .require_audit(ProgressAuditObservation::new(
                ProgressAuditReason::LateCollision,
            ))
            .expect("audit over cancellation");
        let audited_snapshot = audited
            .capture_full_progress_snapshot(&copy_snapshot_intent(&audited))
            .expect("audited snapshot");
        let mut wrong_hold = serde_json::to_value(&audited_snapshot).expect("snapshot value");
        wrong_hold["active_audit"]["suspended_hold"] = json!("CancelRequestedBeforePublish");
        let wrong_hold: FullProgressSnapshotObservation =
            serde_json::from_value(wrong_hold).expect("wrong hold shape");
        assert!(matches!(
            wrong_hold.validate_base(),
            Err(
                FullProgressSnapshotObservationError::CancellationAuditHoldMismatch {
                    route: CancellationRoute::AfterPublish,
                }
            )
        ));

        let mut orphan_hold = serde_json::to_value(&audited_snapshot).expect("snapshot value");
        orphan_hold["cancellation_request"] = serde_json::Value::Null;
        orphan_hold["cancellation_inventory"] = serde_json::Value::Null;
        let orphan_hold: FullProgressSnapshotObservation =
            serde_json::from_value(orphan_hold).expect("orphan hold shape");
        assert!(matches!(
            orphan_hold.validate_base(),
            Err(FullProgressSnapshotObservationError::AuditCancellationHoldMissing)
        ));

        let mut terminal = copy_progress_with_admission_claims();
        terminal
            .request_cancellation(CancellationRequestObservation::new(18124).expect("cancel"))
            .expect("terminal cancellation");
        terminal
            .finish_pre_publish_cancellation(pre_publish_cleanup_authority(
                &terminal,
                PrePublishCleanupObservation::new(Vec::new()).expect("empty cleanup"),
                settlement_inventory(&terminal, AdmissionClaimSettlementOutcome::Released),
            ))
            .expect("terminal cancellation");
        let terminal_snapshot = terminal
            .capture_full_progress_snapshot(&copy_snapshot_intent(&terminal))
            .expect("terminal snapshot");
        let mut orphan_pre_publish =
            serde_json::to_value(&terminal_snapshot).expect("snapshot value");
        orphan_pre_publish["cancellation_request"] = serde_json::Value::Null;
        orphan_pre_publish["cancellation_inventory"] = serde_json::Value::Null;
        orphan_pre_publish["disposition"] = json!("None");
        let orphan_pre_publish: FullProgressSnapshotObservation =
            serde_json::from_value(orphan_pre_publish).expect("orphan prepublish shape");
        assert!(matches!(
            orphan_pre_publish.validate_base(),
            Err(
                FullProgressSnapshotObservationError::CancellationObservationOrphaned {
                    field: "pre_publish_cancellation",
                }
            )
        ));

        let mut after_publish = copy_progress_with_admission_claims();
        publish_prefix(&mut after_publish);
        after_publish
            .request_cancellation(CancellationRequestObservation::new(18125).expect("cancel"))
            .expect("after-publish cancellation");
        let after_publish_snapshot = after_publish
            .capture_full_progress_snapshot(&copy_snapshot_intent(&after_publish))
            .expect("after-publish snapshot");
        let mut invalid_pre_publish_route =
            serde_json::to_value(&after_publish_snapshot).expect("snapshot value");
        invalid_pre_publish_route["pre_publish_cancellation"] =
            serde_json::to_value(terminal_snapshot.pre_publish_cancellation())
                .expect("prepublish observation value");
        let invalid_pre_publish_route: FullProgressSnapshotObservation =
            serde_json::from_value(invalid_pre_publish_route)
                .expect("invalid prepublish route shape");
        assert!(matches!(
            invalid_pre_publish_route.validate_base(),
            Err(
                FullProgressSnapshotObservationError::CancellationPrePublishRouteInvalid {
                    route: CancellationRoute::AfterPublish,
                }
            )
        ));

        let mut with_cleanup = copy_progress_with_admission_claims();
        with_cleanup
            .request_cancellation(CancellationRequestObservation::new(18126).expect("cancel"))
            .expect("cleanup cancellation");
        accept_empty_after_effect_reconciliation(&mut with_cleanup);
        accept_pending_cleanup_observations(&mut with_cleanup);
        let cleanup_snapshot = with_cleanup
            .capture_full_progress_snapshot(&copy_snapshot_intent(&with_cleanup))
            .expect("cleanup snapshot");
        let mut missing_after_effect =
            serde_json::to_value(&cleanup_snapshot).expect("snapshot value");
        missing_after_effect["after_effect_reconciliation"] = serde_json::Value::Null;
        let missing_after_effect: FullProgressSnapshotObservation =
            serde_json::from_value(missing_after_effect).expect("missing after-effect shape");
        assert!(matches!(
            missing_after_effect.validate_base(),
            Err(FullProgressSnapshotObservationError::CancellationCleanupAfterEffectMissing)
        ));

        let mut orphan_after_effect =
            serde_json::to_value(&cleanup_snapshot).expect("snapshot value");
        orphan_after_effect["cancellation_request"] = serde_json::Value::Null;
        orphan_after_effect["cancellation_inventory"] = serde_json::Value::Null;
        orphan_after_effect["disposition"] = json!("None");
        let orphan_after_effect: FullProgressSnapshotObservation =
            serde_json::from_value(orphan_after_effect).expect("orphan after-effect shape");
        assert!(matches!(
            orphan_after_effect.validate_base(),
            Err(
                FullProgressSnapshotObservationError::CancellationObservationOrphaned {
                    field: "after_effect_reconciliation",
                }
            )
        ));
    }

    #[test]
    fn full_progress_snapshot_validates_retry_history_and_clearing() {
        let mut retry = copy_progress_with_admission_claims();
        let retry_observation = RetryObservation::new(
            1,
            100,
            200,
            RetryLeaseId::new("snapshot-retry-state").expect("retry lease"),
            300,
            RetryErrorClass::TransientAvailability,
            "snapshot retry state",
            None,
        )
        .expect("retry observation");
        retry
            .request_retry(retry_observation.clone())
            .expect("request retry");
        let snapshot = retry
            .capture_full_progress_snapshot(&copy_snapshot_intent(&retry))
            .expect("active retry snapshot");
        snapshot.validate_base().expect("valid active retry");

        let mut mismatch = serde_json::to_value(&snapshot).expect("snapshot value");
        mismatch["retry_observation"]["attempt"] = json!(2);
        let mismatch: FullProgressSnapshotObservation =
            serde_json::from_value(mismatch).expect("mismatched retry shape");
        assert!(matches!(
            mismatch.validate_base(),
            Err(FullProgressSnapshotObservationError::RetryObservationMismatch)
        ));

        let mut missing_last = serde_json::to_value(&snapshot).expect("snapshot value");
        missing_last["last_retry_observation"] = serde_json::Value::Null;
        let missing_last: FullProgressSnapshotObservation =
            serde_json::from_value(missing_last).expect("missing last retry shape");
        assert!(matches!(
            missing_last.validate_base(),
            Err(FullProgressSnapshotObservationError::RetryHistoryMissing)
        ));

        let mut missing_active = serde_json::to_value(&snapshot).expect("snapshot value");
        missing_active["retry_observation"] = serde_json::Value::Null;
        let missing_active: FullProgressSnapshotObservation =
            serde_json::from_value(missing_active).expect("missing active retry shape");
        assert!(matches!(
            missing_active.validate_base(),
            Err(FullProgressSnapshotObservationError::RetryPendingObservationMissing)
        ));

        let mut empty_window = serde_json::to_value(&snapshot).expect("snapshot value");
        empty_window["retry_observation"]["lease_expiry_unix_ms"] = json!(200);
        let empty_window: FullProgressSnapshotObservation =
            serde_json::from_value(empty_window).expect("empty retry window shape");
        assert!(matches!(
            empty_window.validate_base(),
            Err(FullProgressSnapshotObservationError::RetryWindowEmpty { slot: "active", .. })
        ));

        let mut malformed = serde_json::to_value(&snapshot).expect("snapshot value");
        malformed["retry_observation"]["attempt"] = json!(0);
        let malformed: FullProgressSnapshotObservation =
            serde_json::from_value(malformed).expect("malformed retry shape");
        assert!(matches!(
            malformed.validate_base(),
            Err(FullProgressSnapshotObservationError::RetryObservation {
                slot: "active",
                source: RetryObservationValidationError::ZeroAttempt,
            })
        ));

        let mut resumed = retry.clone();
        resumed
            .resume_retry(VerifiedRetryResume {
                operation_id: resumed.operation_id,
                plan: resumed.plan.clone(),
                retry_observation,
                now_unix_ms: 200,
                expected_execution_epoch: 0,
            })
            .expect("resume retry");
        let resumed_snapshot = resumed
            .capture_full_progress_snapshot(&copy_snapshot_intent(&resumed))
            .expect("resumed retry snapshot");
        assert!(resumed_snapshot.retry_observation().is_none());
        assert!(resumed_snapshot.last_retry_observation().is_some());
        resumed_snapshot
            .validate_base()
            .expect("cleared active retry remains valid");
        publish_prefix(&mut resumed);
        resumed
            .capture_full_progress_snapshot(&copy_snapshot_intent(&resumed))
            .expect("normal completion after retry resume")
            .validate_base()
            .expect("normal completion clears active retry");
    }

    #[test]
    fn full_progress_snapshot_validates_audit_overlay_and_suspended_holds() {
        let mut audit = copy_progress_with_admission_claims();
        publish_prefix(&mut audit);
        audit
            .require_audit(ProgressAuditObservation::new(
                ProgressAuditReason::RevisionGap,
            ))
            .expect("require audit");
        let snapshot = audit
            .capture_full_progress_snapshot(&copy_snapshot_intent(&audit))
            .expect("active audit snapshot");
        snapshot.validate_base().expect("valid active audit");

        let mut missing = serde_json::to_value(&snapshot).expect("snapshot value");
        missing["active_audit"] = serde_json::Value::Null;
        let missing: FullProgressSnapshotObservation =
            serde_json::from_value(missing).expect("missing audit shape");
        assert!(matches!(
            missing.validate_base(),
            Err(FullProgressSnapshotObservationError::AuditStateMissing)
        ));

        let mut forbidden = serde_json::to_value(&snapshot).expect("snapshot value");
        forbidden["disposition"] = json!("None");
        let forbidden: FullProgressSnapshotObservation =
            serde_json::from_value(forbidden).expect("forbidden audit disposition shape");
        assert!(matches!(
            forbidden.validate_base(),
            Err(
                FullProgressSnapshotObservationError::AuditDispositionMismatch {
                    disposition: FullProgressDisposition::None,
                }
            )
        ));

        let mut zero_generation = serde_json::to_value(&snapshot).expect("snapshot value");
        zero_generation["active_audit"]["generation"] = json!(0);
        let zero_generation: FullProgressSnapshotObservation =
            serde_json::from_value(zero_generation).expect("zero audit generation shape");
        assert!(matches!(
            zero_generation.validate_base(),
            Err(FullProgressSnapshotObservationError::AuditGenerationZero)
        ));

        let mut generation_mismatch = serde_json::to_value(&snapshot).expect("snapshot value");
        generation_mismatch["active_audit"]["generation"] = json!(2);
        let generation_mismatch: FullProgressSnapshotObservation =
            serde_json::from_value(generation_mismatch).expect("audit generation shape");
        assert!(matches!(
            generation_mismatch.validate_base(),
            Err(
                FullProgressSnapshotObservationError::AuditGenerationMismatch {
                    active: 2,
                    retained: 1,
                }
            )
        ));

        let mut invalid_target = serde_json::to_value(&snapshot).expect("snapshot value");
        invalid_target["active_audit"]["observation"]["target"] = json!({ "Steps": [] });
        let invalid_target: FullProgressSnapshotObservation =
            serde_json::from_value(invalid_target).expect("invalid audit target shape");
        assert!(matches!(
            invalid_target.validate_base(),
            Err(FullProgressSnapshotObservationError::AuditObservation(
                AuditTargetValidationError::Empty
            ))
        ));

        let mut retry = copy_progress_with_admission_claims();
        publish_prefix(&mut retry);
        let retry_observation = RetryObservation::new(
            1,
            0,
            1,
            RetryLeaseId::new("snapshot-audit-hold").expect("retry lease"),
            10,
            RetryErrorClass::TransientContention,
            "audit hold",
            None,
        )
        .expect("retry observation");
        retry
            .request_retry(retry_observation.clone())
            .expect("request retry");
        retry
            .require_audit(ProgressAuditObservation::new(
                ProgressAuditReason::UncertainEvidence,
            ))
            .expect("audit over retry");
        retry
            .capture_full_progress_snapshot(&copy_snapshot_intent(&retry))
            .expect("retry audit snapshot")
            .validate_base()
            .expect("retry remains retained under audit");
        retry
            .request_cancellation(CancellationRequestObservation::new(18102).expect("cancel"))
            .expect("cancellation under audit");
        retry
            .resolve_audit(audit_resolution_authority(&retry, 1, 0))
            .expect("resolve retry cancellation audit");
        assert_eq!(
            retry.disposition(),
            FullProgressDisposition::CancelRequestedAfterPublish
        );
        retry
            .capture_full_progress_snapshot(&copy_snapshot_intent(&retry))
            .expect("retry retained after audit resolution")
            .validate_base()
            .expect("retry remains retained under cancellation");

        let mut resolved = copy_progress_with_admission_claims();
        publish_prefix(&mut resolved);
        resolved
            .require_audit(ProgressAuditObservation::new(
                ProgressAuditReason::RevisionGap,
            ))
            .expect("require audit for history");
        resolved
            .resolve_audit(audit_resolution_authority(&resolved, 1, 0))
            .expect("resolve audit for history");
        let resolved_snapshot = resolved
            .capture_full_progress_snapshot(&copy_snapshot_intent(&resolved))
            .expect("historical audit generation snapshot");
        assert!(resolved_snapshot.audit_observation().is_none());
        assert_eq!(resolved_snapshot.audit_generation(), 1);
        resolved_snapshot
            .validate_base()
            .expect("historical audit generation without active overlay");
    }

    #[test]
    fn snapshot_nested_observation_payloads_reject_unknown_fields() {
        fn assert_unknown_payload_rejected<T>(value: T)
        where
            T: serde::Serialize + serde::de::DeserializeOwned,
        {
            let mut encoded = serde_json::to_value(value).expect("nested observation JSON");
            encoded
                .as_object_mut()
                .expect("externally tagged observation")
                .values_mut()
                .next()
                .expect("observation payload")
                .as_object_mut()
                .expect("struct observation payload")
                .insert("unknown_nested_field".to_owned(), json!(true));
            assert!(serde_json::from_value::<T>(encoded).is_err());
        }

        let progress = copy_progress_with_admission_claims();
        let source_key = key_for(&progress, 0, WorkUnitRole::SourceRead);
        let stage_key = key_for(&progress, 0, WorkUnitRole::Stage);
        let publish_key = key_for(&progress, 0, WorkUnitRole::Publish);
        let coordinator_key = progress
            .steps()
            .iter()
            .find(|step| expected_coordinator_receipt(step.key().role()).is_some())
            .map(WorkProgressStep::key)
            .expect("coordinator work-plan role");

        assert_unknown_payload_rejected(CancellationSourceClosureOutcome::Observed {
            evidence: evidence_for(&progress, source_key, 1),
            context: CompletionContext::new(
                progress.execution_epoch(),
                progress
                    .scope_for(source_key)
                    .expect("source scope")
                    .clone(),
            ),
        });
        assert_unknown_payload_rejected(CancellationCoordinatorClosureOutcome::Observed {
            evidence: evidence_for(&progress, coordinator_key, 1),
            context: CompletionContext::new(
                progress.execution_epoch(),
                progress
                    .scope_for(coordinator_key)
                    .expect("coordinator scope")
                    .clone(),
            ),
        });
        assert_unknown_payload_rejected(CancellationResourceCleanupOutcome::Preserved {
            owner_revision: 1,
            context: CompletionContext::new(
                progress.execution_epoch(),
                progress.scope_for(stage_key).expect("stage scope").clone(),
            ),
        });
        assert_unknown_payload_rejected(CancellationNamespaceCleanupOutcome::Preserved {
            owner_revision: 1,
            context: CompletionContext::new(
                progress.execution_epoch(),
                progress
                    .scope_for(publish_key)
                    .expect("publish scope")
                    .clone(),
            ),
        });
    }

    #[test]
    fn full_progress_snapshot_base_rejects_binding_and_step_corruption() {
        let progress = copy_progress_with_admission_claims();
        let snapshot = progress
            .capture_full_progress_snapshot(&copy_snapshot_intent(&progress))
            .expect("snapshot");

        let intent = copy_snapshot_intent(&progress);
        let other_progress = WorkProgress::from_plan_with_admission_observations(
            OperationId::from_uuid(Uuid::from_u128(1205)),
            progress.plan(),
            intent.args(),
            intent.participants(),
            &admission_capacity_plan(),
            admission_claims(12),
        )
        .expect("other progress");
        let other_intent = copy_snapshot_intent(&other_progress);
        assert!(matches!(
            progress.capture_full_progress_snapshot(&other_intent),
            Err(FullProgressSnapshotObservationError::OperationIdMismatch)
        ));

        let mut reordered = serde_json::to_value(&snapshot).expect("snapshot value");
        reordered
            .get_mut("steps")
            .and_then(serde_json::Value::as_array_mut)
            .expect("steps array")
            .swap(0, 1);
        let reordered: FullProgressSnapshotObservation =
            serde_json::from_value(reordered).expect("reordered snapshot shape");
        assert!(matches!(
            reordered.validate_base(),
            Err(FullProgressSnapshotObservationError::StepKeyMismatch { .. })
        ));

        let mut deferred = serde_json::to_value(&snapshot).expect("snapshot value");
        deferred
            .get_mut("steps")
            .and_then(serde_json::Value::as_array_mut)
            .expect("steps array")[0]
            .as_object_mut()
            .expect("step object")
            .insert("status".to_owned(), json!("Deferred"));
        let deferred: FullProgressSnapshotObservation =
            serde_json::from_value(deferred).expect("deferred snapshot shape");
        assert!(matches!(
            deferred.validate_base(),
            Err(FullProgressSnapshotObservationError::DeferredRoleInvalid { .. })
                | Err(FullProgressSnapshotObservationError::StepApplicabilityMismatch { .. })
        ));
    }

    #[test]
    fn full_progress_snapshot_base_rejects_history_and_dependency_corruption() {
        let mut progress = copy_progress_with_admission_claims();
        publish_prefix(&mut progress);
        let snapshot = progress
            .capture_full_progress_snapshot(&copy_snapshot_intent(&progress))
            .expect("completed snapshot");

        let mut missing_evidence = serde_json::to_value(&snapshot).expect("snapshot value");
        missing_evidence
            .get_mut("steps")
            .and_then(serde_json::Value::as_array_mut)
            .expect("steps array")
            .iter_mut()
            .find(|step| step["key"]["role"] == json!("SourceRead"))
            .expect("source step")
            .as_object_mut()
            .expect("source step object")
            .insert("evidence".to_owned(), serde_json::Value::Null);
        let missing_evidence: FullProgressSnapshotObservation =
            serde_json::from_value(missing_evidence).expect("missing evidence shape");
        assert!(matches!(
            missing_evidence.validate_base(),
            Err(FullProgressSnapshotObservationError::Completion {
                source: WorkProgressError::CompletionEvidenceMissing { .. }
            })
        ));

        let mut future_context = serde_json::to_value(&snapshot).expect("snapshot value");
        future_context
            .get_mut("steps")
            .and_then(serde_json::Value::as_array_mut)
            .expect("steps array")
            .iter_mut()
            .find(|step| step["key"]["role"] == json!("SourceRead"))
            .expect("source step")
            .get_mut("completion_context")
            .expect("completion context")
            .as_object_mut()
            .expect("completion context object")
            .insert("execution_epoch".to_owned(), json!(1));
        let future_context: FullProgressSnapshotObservation =
            serde_json::from_value(future_context).expect("future context shape");
        assert!(matches!(
            future_context.validate_base(),
            Err(FullProgressSnapshotObservationError::Completion {
                source: WorkProgressError::CompletionContextEpochFuture { .. }
            })
        ));

        let mut wrong_requirement = serde_json::to_value(&snapshot).expect("snapshot value");
        wrong_requirement
            .get_mut("steps")
            .and_then(serde_json::Value::as_array_mut)
            .expect("steps array")
            .iter_mut()
            .find(|step| step["key"]["role"] == json!("SourceRead"))
            .expect("source step")
            .as_object_mut()
            .expect("source step object")
            .insert("requirement".to_owned(), json!("OptionalRebuildable"));
        let wrong_requirement: FullProgressSnapshotObservation =
            serde_json::from_value(wrong_requirement).expect("wrong requirement shape");
        assert!(matches!(
            wrong_requirement.validate_base(),
            Err(FullProgressSnapshotObservationError::StepRequirementMismatch { .. })
        ));

        let mut dependency_hole = serde_json::to_value(&snapshot).expect("snapshot value");
        let stage = dependency_hole
            .get_mut("steps")
            .and_then(serde_json::Value::as_array_mut)
            .expect("steps array")
            .iter_mut()
            .find(|step| step["key"]["role"] == json!("Stage"))
            .expect("stage step")
            .as_object_mut()
            .expect("stage step object");
        stage.insert("status".to_owned(), json!("Pending"));
        stage.insert("evidence".to_owned(), serde_json::Value::Null);
        stage.insert("completion_context".to_owned(), serde_json::Value::Null);
        let dependency_hole: FullProgressSnapshotObservation =
            serde_json::from_value(dependency_hole).expect("dependency hole shape");
        assert!(matches!(
            dependency_hole.validate_base(),
            Err(FullProgressSnapshotObservationError::PrerequisiteIncomplete { .. })
        ));
    }
}
