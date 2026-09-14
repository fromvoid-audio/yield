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
use thiserror::Error;

use super::admission_claims::{AdmissionClaimInventory, AdmissionClaimValidationError};
use super::cancellation_recovery::{
    CancellationInventory, CancellationInventoryError, CancellationRoute,
};
use super::coordinator::{RetryObservation, RetryObservationValidationError, UniversalCommandArgs};
use super::full_status::FullProgressDisposition;
use super::model::{
    CancellationRequestObservation, CancellationRequestValidationError, OperationId, OperationPhase,
};
use super::participants::{
    MAX_SOURCE_IDENTITY_BYTES, ParticipantDescriptor, ParticipantId, ParticipantKind,
    ParticipantRequirement, ParticipantSet, ParticipantTextField, ParticipantTextViolation,
    ParticipantValidationError, StoredSourceFence,
};
use super::work_plan::{
    MAX_WORK_PLAN_UNITS, WorkPlan, WorkPlanError, WorkPrerequisite, WorkUnitApplicability,
    WorkUnitKey, WorkUnitRole,
};
use crate::durable_capacity::DurableCapacityPlan;
use crate::sample_sources::SourceId;

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
}

impl StoredStepEvidence {
    /// Validate nested evidence shape without treating it as completion authority.
    pub fn validate(&self) -> Result<(), ProgressEvidenceError> {
        match self {
            Self::SourceObserved { observation } => observation.validate(),
            Self::SourceCommitted { after, .. } => after.validate(),
            Self::Filesystem { .. } => Ok(()),
            Self::Coordinator { receipt } => receipt.validate(),
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
#[derive(Clone, Debug, Eq, PartialEq)]
struct ActiveAudit {
    generation: u64,
    observation: ProgressAuditObservation,
    suspended_hold: AuditSuspendedHold,
}

/// Immutable participant scope captured from the initial participant inventory.
///
/// A scope is an observation of participant kind and source fence.  It is not a lease, owner
/// capability, filesystem root, or permission to perform the role.
#[derive(Clone, Debug, Eq, PartialEq)]
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkProgressStep {
    key: WorkUnitKey,
    requirement: ParticipantRequirement,
    status: ProgressStepStatus,
    evidence: Option<StoredStepEvidence>,
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

/// Legacy placeholder for the not-yet-implemented pre-publication cancellation outcome.
#[allow(dead_code)]
struct PrePublishCancellationAuthority {
    operation_id: OperationId,
    plan: WorkPlan,
    _staging_absence: VerifiedStagingAbsence,
    _capacity_release: VerifiedCapacityRelease,
}

/// Placeholder marker retained until the cancellation outcome gate is implemented.
#[allow(dead_code)]
struct VerifiedStagingAbsence;

/// Placeholder marker retained until the cancellation outcome gate is implemented.
#[allow(dead_code)]
struct VerifiedCapacityRelease;

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
    cancellation_inventory: Option<CancellationInventory>,
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
            cancellation_inventory: None,
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
        capacity_plan: &DurableCapacityPlan,
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

    /// Borrow the immutable cancellation inventory frozen by the first request, when present.
    pub fn cancellation_inventory(&self) -> Option<&CancellationInventory> {
        self.cancellation_inventory.as_ref()
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

    /// Defer one optional artifact while retaining required-progress guards.
    pub(crate) fn defer_optional_artifact(
        &mut self,
        key: WorkUnitKey,
    ) -> Result<(), WorkProgressError> {
        if self.disposition != FullProgressDisposition::None {
            return Err(WorkProgressError::ProgressSuspended {
                disposition: self.disposition,
            });
        }
        let step = self
            .steps
            .iter_mut()
            .find(|step| step.key == key)
            .ok_or(WorkProgressError::UnknownWorkUnit { key })?;
        if step.key.role() != WorkUnitRole::Artifact
            || step.requirement != ParticipantRequirement::OptionalRebuildable
            || step.status != ProgressStepStatus::Pending
        {
            return Err(WorkProgressError::ArtifactDeferralForbidden { key });
        }
        step.status = ProgressStepStatus::Deferred;
        self.refresh_derived_state();
        Ok(())
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
        let current_status = self.steps[index].status;
        if current_status == ProgressStepStatus::Completed {
            if self.steps[index].evidence.as_ref() == Some(&completion.evidence) {
                return Ok(ProgressApplyOutcome::Replay);
            }
            return Err(WorkProgressError::CompletionReplayConflict {
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
        validate_evidence_for_role(
            completion.key.role(),
            expected_scope,
            &completion.evidence,
            minimum_revision,
        )?;
        self.steps[index].status = ProgressStepStatus::Completed;
        self.steps[index].evidence = Some(completion.evidence);
        if self.disposition == FullProgressDisposition::RetryPending {
            self.disposition = FullProgressDisposition::None;
            self.retry_observation = None;
        }
        self.refresh_derived_state();
        Ok(ProgressApplyOutcome::Applied)
    }

    /// Cancellation outcomes remain unavailable until cleanup, settlement, and safe-state
    /// authorities are implemented.  The placeholder authority cannot advance progress.
    #[allow(dead_code)]
    fn finish_pre_publish_cancellation(
        &mut self,
        _authority: PrePublishCancellationAuthority,
    ) -> Result<(), WorkProgressError> {
        Err(WorkProgressError::CancellationOutcomeUnavailable)
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

    #[allow(dead_code)]
    fn scope_for(&self, key: WorkUnitKey) -> Option<&ParticipantScope> {
        self.scopes
            .iter()
            .find(|scope| scope.participant_id == key.participant_id())
    }

    fn minimum_source_revision_for_fence(&self, fence: &StoredSourceFence) -> Option<u64> {
        self.steps
            .iter()
            .filter(|step| {
                self.scope_for(step.key)
                    .and_then(ParticipantScope::source_fence)
                    .is_some_and(|candidate| candidate == fence)
            })
            .filter_map(|step| match step.evidence.as_ref() {
                Some(StoredStepEvidence::SourceObserved { observation })
                | Some(StoredStepEvidence::SourceCommitted {
                    after: observation, ..
                }) => Some(observation.revision()),
                Some(StoredStepEvidence::Coordinator { receipt }) => receipt
                    .source_observation()
                    .map(SourceRevisionObservation::revision),
                _ => None,
            })
            .max()
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
            let Some(scope_fence) = self
                .scope_for(step.key)
                .and_then(ParticipantScope::source_fence)
            else {
                continue;
            };
            if !same_stable_source_identity(scope_fence, fence) {
                continue;
            }
            let observation = match step.evidence.as_ref() {
                Some(StoredStepEvidence::SourceObserved { observation })
                | Some(StoredStepEvidence::SourceCommitted {
                    after: observation, ..
                }) => Some(observation),
                Some(StoredStepEvidence::Coordinator { receipt }) => receipt.source_observation(),
                _ => None,
            };
            if let Some(observation) = observation {
                if same_stable_source_identity_observation(observation, fence) {
                    minimum = minimum.max(observation.revision());
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
        if self.all_required_complete()
            && self.active_audit.is_none()
            && self.optional_artifacts_resolved()
        {
            self.phase = OperationPhase::Terminal;
            self.disposition = if self.has_deferred_optional_artifact() {
                FullProgressDisposition::SucceededWithDeferredArtifacts
            } else {
                FullProgressDisposition::Succeeded
            };
        }
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

fn same_stable_source_identity_observation(
    observation: &SourceRevisionObservation,
    fence: &StoredSourceFence,
) -> bool {
    observation.source_id() == fence.source_id()
        && observation.root_identity() == fence.root_identity()
        && observation.database_identity() == fence.database_identity()
        && observation.lifecycle_generation() == fence.lifecycle_generation()
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
    /// A nested evidence observation was malformed.
    #[error("invalid work progress evidence: {0}")]
    Evidence(#[from] ProgressEvidenceError),
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
    /// The evidence role did not match the exact work role.
    #[error("work progress evidence role does not match {role:?}")]
    RoleEvidenceMismatch {
        /// Work role expected by the key.
        role: WorkUnitRole,
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
    use crate::durable_capacity::{
        CapacityAllocationClass, DurableCapacityPlan, DurableVolumeCapacity,
        PROTECTED_FREE_SPACE_FLOOR, VolumeIdentity,
    };
    use crate::operation_journal::MAX_CANCELLATION_STEPS;
    use crate::operation_journal::{
        AdmissionClaim, AdmissionClaimId, AdmissionClaimInventory, CancellationActionState,
        CopyCollisionPolicy, CopyEndpoint, CopyIntentArgs, CopySourceExpectation, EvidenceBatchId,
        ExternalCaptureBoundary, ExternalNativeString, ExternalRawEventKind,
        ExternalRawNativeAccounting, ExternalRawObservation, ExternalRawObservationEnvelope,
        ExternalRawObservationLimits, ExternalRawObservationMetadata,
        ExternalRawObservationProvenance, ExternalRawObservedPath, ExternalRawPathRole,
        ExternalReconciliationIntentArgs, ExtractAndTrimIntentArgs, FiniteF32Bits,
        NamespaceCommandArgs, NamespaceDirectoryEndpoint, NamespaceDirectoryExpectation,
        NamespaceDirectoryLocator, NamespaceEntryEndpoint, NamespaceEntryExpectation,
        NamespaceFileExpectation, NamespaceLeafName, NamespaceRenameIntentArgs, NamespaceTarget,
        ParticipantCheckpoint, ParticipantCheckpointState, RawWavExtractionRecipe,
        RetainedDeleteId, RetainedFolderObservation, RetainedPurgeDecision,
        RetainedPurgeIntentArgs, RetainedRecoveryRootKind, RetainedResolutionCommandArgs,
        RetainedResolutionStage, RetainedRestoreIntentArgs, RetryErrorClass, RetryLeaseId,
        SelectionExtractIntentArgs, SelectionOutputCommandArgs, SelectionOutputRecipe,
        StoredRelativePath, UniversalCommandArgs, WaveformEditFrameRange,
        WaveformFileRevisionObservation, WaveformOutputSpec, WaveformSampleFormat,
        WaveformSourceSpec, fingerprint_selected_entry_json,
    };
    use crate::sample_sources::{SourceId, SourcePlaybackType};
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

    fn admission_capacity_plan() -> DurableCapacityPlan {
        DurableCapacityPlan {
            volumes: vec![DurableVolumeCapacity {
                identity: VolumeIdentity { device: 1 },
                allocation_unit: 1,
                allocation_class: CapacityAllocationClass::DestinationStaging,
                logical_bytes: 12,
                allocated_bytes: 12,
                protected_free_bytes: PROTECTED_FREE_SPACE_FLOOR,
            }],
        }
    }

    fn admission_claims(allocated_bytes: u64) -> AdmissionClaimInventory {
        AdmissionClaimInventory::new(vec![
            AdmissionClaim::new(
                AdmissionClaimId::from_uuid(Uuid::from_u128(201)).expect("claim id"),
                VolumeIdentity { device: 1 },
                CapacityAllocationClass::DestinationStaging,
                allocated_bytes,
            )
            .expect("claim"),
        ])
        .expect("claim inventory")
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

    fn key_for(progress: &WorkProgress, action: u8, role: WorkUnitRole) -> WorkUnitKey {
        progress
            .steps()
            .iter()
            .find(|step| step.key().action_ordinal() == action && step.key().role() == role)
            .map(WorkProgressStep::key)
            .expect("work-plan role")
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

    fn complete_required(progress: &mut WorkProgress) {
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
            let evidence = evidence_for(progress, key, 7);
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
        let stale_readiness =
            CoordinatorReceiptSummary::new(CoordinatorReceiptKind::Readiness, 1, None)
                .with_source_observation(source_observation(&progress, readiness, 7));
        assert!(matches!(
            progress.apply_verified_completion(completion(
                &progress,
                readiness,
                StoredStepEvidence::Coordinator {
                    receipt: stale_readiness,
                },
            )),
            Err(WorkProgressError::SourceRevisionRegressed {
                role: WorkUnitRole::Readiness,
                observed: 7,
                minimum: 8,
            })
        ));
        let current_readiness =
            CoordinatorReceiptSummary::new(CoordinatorReceiptKind::Readiness, 1, None)
                .with_source_observation(source_observation(&progress, readiness, 8));
        progress
            .apply_verified_completion(completion(
                &progress,
                readiness,
                StoredStepEvidence::Coordinator {
                    receipt: current_readiness,
                },
            ))
            .expect("current readiness revision");

        let unfenced =
            CoordinatorReceiptSummary::new(CoordinatorReceiptKind::GlobalLibrary, 1, None);
        assert!(unfenced.source_observation().is_none());
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
        progress
            .defer_optional_artifact(artifact_key)
            .expect("defer optional artifact");
        assert_eq!(progress.phase(), OperationPhase::Terminal);
        assert_eq!(
            progress.disposition(),
            FullProgressDisposition::SucceededWithDeferredArtifacts
        );

        let mut required_only = copy_progress(false);
        complete_required(&mut required_only);
        assert_eq!(required_only.phase(), OperationPhase::Terminal);
        assert_eq!(
            required_only.disposition(),
            FullProgressDisposition::Succeeded
        );
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
        let authority = PrePublishCancellationAuthority {
            operation_id: cancelled.operation_id,
            plan: cancelled.plan.clone(),
            _staging_absence: VerifiedStagingAbsence,
            _capacity_release: VerifiedCapacityRelease,
        };
        let before_outcome = cancelled.clone();
        assert_eq!(
            cancelled.finish_pre_publish_cancellation(authority),
            Err(WorkProgressError::CancellationOutcomeUnavailable)
        );
        assert_eq!(cancelled, before_outcome);
        assert_eq!(
            cancelled.disposition(),
            FullProgressDisposition::CancelRequestedBeforePublish
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

        let mut terminal = copy_progress(false);
        complete_required(&mut terminal);
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
}
