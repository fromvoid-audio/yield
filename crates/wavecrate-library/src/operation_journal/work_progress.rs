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

use super::coordinator::{RetryObservation, RetryObservationValidationError, UniversalCommandArgs};
use super::model::{
    CancellationRequestObservation, CancellationRequestValidationError, OperationDisposition,
    OperationId, OperationPhase,
};
use super::participants::{
    MAX_SOURCE_IDENTITY_BYTES, ParticipantDescriptor, ParticipantId, ParticipantKind,
    ParticipantRequirement, ParticipantSet, ParticipantTextField, ParticipantTextViolation,
    ParticipantValidationError, StoredSourceFence,
};
use super::work_plan::{
    WorkPlan, WorkPlanError, WorkPrerequisite, WorkUnitApplicability, WorkUnitKey, WorkUnitRole,
};
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

/// Bounded audit observation retained when progress must stop and preserve evidence.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressAuditObservation {
    reason: ProgressAuditReason,
}

impl ProgressAuditObservation {
    /// Construct an audit observation without resolving the audit.
    pub const fn new(reason: ProgressAuditReason) -> Self {
        Self { reason }
    }

    /// Return the closed audit reason.
    pub const fn reason(self) -> ProgressAuditReason {
        self.reason
    }
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

/// Internal typed gate for a pre-publication cancellation terminal transition.
#[allow(dead_code)]
struct PrePublishCancellationAuthority {
    operation_id: OperationId,
    plan: WorkPlan,
    _staging_absence: VerifiedStagingAbsence,
    _capacity_release: VerifiedCapacityRelease,
}

/// Internal marker for capability-bound verified staging absence.
#[allow(dead_code)]
struct VerifiedStagingAbsence;

/// Internal marker for durable capacity-release evidence.
#[allow(dead_code)]
struct VerifiedCapacityRelease;

/// Internal typed gate for a post-publication cancellation terminal transition.
#[allow(dead_code)]
struct PostPublishCancellationAuthority {
    operation_id: OperationId,
    plan: WorkPlan,
    _recovery: VerifiedPostPublishRecovery,
}

/// Internal marker for complete verified post-publication reconciliation or compensation.
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
    disposition: OperationDisposition,
    retry_observation: Option<RetryObservation>,
    last_retry_observation: Option<RetryObservation>,
    execution_epoch: u64,
    audit_observation: Option<ProgressAuditObservation>,
    cancellation_request: Option<CancellationRequestObservation>,
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
            disposition: OperationDisposition::None,
            retry_observation: None,
            last_retry_observation: None,
            execution_epoch: 0,
            audit_observation: None,
            cancellation_request: None,
        })
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
    pub const fn disposition(&self) -> OperationDisposition {
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
    pub const fn audit_observation(&self) -> Option<ProgressAuditObservation> {
        self.audit_observation
    }

    /// Borrow the retained cancellation request, when cancellation was requested.
    pub const fn cancellation_request(&self) -> Option<CancellationRequestObservation> {
        self.cancellation_request
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
            OperationDisposition::AuditRequired
                | OperationDisposition::CancelRequestedBeforePublish
                | OperationDisposition::CancelRequestedAfterPublish
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
        self.disposition = OperationDisposition::RetryPending;
        self.retry_observation = Some(observation);
        Ok(())
    }

    /// Resume a retry only after a private owner has reacquired its lease in the valid window.
    #[allow(dead_code)]
    fn resume_retry(&mut self, authority: VerifiedRetryResume) -> Result<(), WorkProgressError> {
        if self.disposition != OperationDisposition::RetryPending {
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
        self.disposition = OperationDisposition::None;
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
        self.audit_observation = Some(observation);
        self.disposition = OperationDisposition::AuditRequired;
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
        if self.disposition == OperationDisposition::AuditRequired {
            return Err(WorkProgressError::ProgressSuspended {
                disposition: self.disposition,
            });
        }
        if let Some(existing) = self.cancellation_request {
            if existing == observation {
                return Ok(());
            }
            return Err(WorkProgressError::CancellationReplayConflict);
        }
        self.cancellation_request = Some(observation);
        self.disposition = if self.phase.is_pre_publication() {
            OperationDisposition::CancelRequestedBeforePublish
        } else if self.phase.is_post_publication() {
            OperationDisposition::CancelRequestedAfterPublish
        } else {
            return Err(WorkProgressError::CancellationPhaseInvalid);
        };
        Ok(())
    }

    /// Defer one optional artifact while retaining required-progress guards.
    pub(crate) fn defer_optional_artifact(
        &mut self,
        key: WorkUnitKey,
    ) -> Result<(), WorkProgressError> {
        if self.disposition != OperationDisposition::None {
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
        if self.disposition == OperationDisposition::RetryPending {
            return Err(WorkProgressError::ProgressSuspended {
                disposition: self.disposition,
            });
        }
        if self.disposition != OperationDisposition::None
            && self.disposition != OperationDisposition::RetryPending
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
        if self.disposition == OperationDisposition::RetryPending {
            self.disposition = OperationDisposition::None;
            self.retry_observation = None;
        }
        self.refresh_derived_state();
        Ok(ProgressApplyOutcome::Applied)
    }

    /// Complete a pre-publication cancellation only with an internal cleanup/release gate.
    #[allow(dead_code)]
    fn finish_pre_publish_cancellation(
        &mut self,
        authority: PrePublishCancellationAuthority,
    ) -> Result<(), WorkProgressError> {
        if self.disposition != OperationDisposition::CancelRequestedBeforePublish {
            return Err(WorkProgressError::CancellationNotRequested);
        }
        if authority.operation_id != self.operation_id || authority.plan != self.plan {
            return Err(WorkProgressError::CancellationAuthorityMismatch);
        }
        self.phase = OperationPhase::Terminal;
        self.disposition = OperationDisposition::CancelledBeforePublish;
        Ok(())
    }

    /// Complete a post-publication cancellation only with an internal recovery gate.
    #[allow(dead_code)]
    fn finish_post_publish_cancellation(
        &mut self,
        authority: PostPublishCancellationAuthority,
    ) -> Result<(), WorkProgressError> {
        if self.disposition != OperationDisposition::CancelRequestedAfterPublish {
            return Err(WorkProgressError::CancellationNotRequested);
        }
        if authority.operation_id != self.operation_id || authority.plan != self.plan {
            return Err(WorkProgressError::CancellationAuthorityMismatch);
        }
        self.phase = OperationPhase::Terminal;
        self.disposition = OperationDisposition::CancelledAfterPublish;
        Ok(())
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
                OperationDisposition::AuditRequired
                    | OperationDisposition::CancelRequestedBeforePublish
                    | OperationDisposition::CancelRequestedAfterPublish
            )
        {
            return;
        }
        let candidate = self.derived_phase();
        if phase_rank(candidate) > phase_rank(self.phase) {
            self.phase = candidate;
        }
        if self.all_required_complete()
            && self.audit_observation.is_none()
            && self.optional_artifacts_resolved()
        {
            self.phase = OperationPhase::Terminal;
            self.disposition = if self.has_deferred_optional_artifact() {
                OperationDisposition::SucceededWithDeferredArtifacts
            } else {
                OperationDisposition::Succeeded
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
    /// A nested evidence observation was malformed.
    #[error("invalid work progress evidence: {0}")]
    Evidence(#[from] ProgressEvidenceError),
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
        disposition: OperationDisposition,
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
    use crate::operation_journal::{
        CopyCollisionPolicy, CopyEndpoint, CopyIntentArgs, CopySourceExpectation,
        ExtractAndTrimIntentArgs, FiniteF32Bits, NamespaceCommandArgs, NamespaceDirectoryEndpoint,
        NamespaceDirectoryExpectation, NamespaceDirectoryLocator, NamespaceEntryEndpoint,
        NamespaceEntryExpectation, NamespaceFileExpectation, NamespaceLeafName,
        NamespaceRenameIntentArgs, NamespaceTarget, ParticipantCheckpoint,
        ParticipantCheckpointState, RawWavExtractionRecipe, RetryErrorClass, RetryLeaseId,
        SelectionExtractIntentArgs, SelectionOutputCommandArgs, SelectionOutputRecipe,
        StoredRelativePath, UniversalCommandArgs, WaveformEditFrameRange,
        WaveformFileRevisionObservation, WaveformOutputSpec, WaveformSampleFormat,
        WaveformSourceSpec,
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

    fn apply_role(progress: &mut WorkProgress, action: u8, role: WorkUnitRole) {
        let key = key_for(progress, action, role);
        let evidence = evidence_for(progress, key, 7);
        progress
            .apply_verified_completion(completion(progress, key, evidence))
            .expect("completion applies");
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
        let mut progress = WorkProgress::from_plan(
            OperationId::from_uuid(Uuid::from_u128(103)),
            &plan,
            &args,
            &participants,
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
        assert_eq!(progress.disposition(), OperationDisposition::None);
    }

    #[test]
    fn required_completion_and_optional_artifact_deferral_have_distinct_terminals() {
        let mut progress = copy_progress(true);
        complete_required(&mut progress);
        assert_eq!(progress.phase(), OperationPhase::ReadinessScheduled);
        assert_eq!(progress.disposition(), OperationDisposition::None);
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
            OperationDisposition::SucceededWithDeferredArtifacts
        );

        let mut required_only = copy_progress(false);
        complete_required(&mut required_only);
        assert_eq!(required_only.phase(), OperationPhase::Terminal);
        assert_eq!(required_only.disposition(), OperationDisposition::Succeeded);
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
                disposition: OperationDisposition::RetryPending
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
        assert_eq!(retry.disposition(), OperationDisposition::None);
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
        assert_eq!(retry.disposition(), OperationDisposition::None);
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
        audit
            .require_audit(ProgressAuditObservation::new(
                ProgressAuditReason::UncertainEvidence,
            ))
            .expect("require audit");
        let audit_key = key_for(&audit, 0, WorkUnitRole::SourceRead);
        assert!(matches!(
            audit.apply_verified_completion(completion(
                &audit,
                audit_key,
                evidence_for(&audit, audit_key, 7),
            )),
            Err(WorkProgressError::ProgressSuspended {
                disposition: OperationDisposition::AuditRequired
            })
        ));

        let mut cancelled = copy_progress(false);
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
        let wrong_authority = PrePublishCancellationAuthority {
            operation_id: OperationId::from_uuid(Uuid::from_u128(104)),
            plan: cancelled.plan.clone(),
            _staging_absence: VerifiedStagingAbsence,
            _capacity_release: VerifiedCapacityRelease,
        };
        assert!(matches!(
            cancelled.finish_pre_publish_cancellation(wrong_authority),
            Err(WorkProgressError::CancellationAuthorityMismatch)
        ));
        let authority = PrePublishCancellationAuthority {
            operation_id: cancelled.operation_id,
            plan: cancelled.plan.clone(),
            _staging_absence: VerifiedStagingAbsence,
            _capacity_release: VerifiedCapacityRelease,
        };
        cancelled
            .finish_pre_publish_cancellation(authority)
            .expect("guarded cancellation terminal");
        assert_eq!(cancelled.phase(), OperationPhase::Terminal);
        assert_eq!(
            cancelled.disposition(),
            OperationDisposition::CancelledBeforePublish
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
        assert_eq!(progress.disposition(), OperationDisposition::None);
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
        assert_eq!(retry.disposition(), OperationDisposition::RetryPending);
        assert_eq!(retry.retry_observation(), Some(&observation));

        let mut audit = copy_progress(false);
        let target = key_for(&audit, 0, WorkUnitRole::SourceRead).participant_id();
        let audit_observation = ProgressAuditObservation::new(ProgressAuditReason::RevisionGap);
        audit
            .require_audit(audit_observation)
            .expect("require audit");
        audit
            .reacquire_source(source_reacquisition_authority(&audit, target, 7, 2, 0))
            .expect("reacquire while audit required");
        assert_eq!(audit.execution_epoch(), 1);
        assert_eq!(audit.disposition(), OperationDisposition::AuditRequired);
        assert_eq!(audit.audit_observation(), Some(audit_observation));

        let mut cancelled = copy_progress(false);
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
            OperationDisposition::CancelRequestedBeforePublish
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
}
