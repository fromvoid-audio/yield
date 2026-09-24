//! Bounded cancellation observations captured from guarded full progress.
//!
//! A cancellation inventory freezes the exact progress snapshot and the derived action states
//! at the first cancellation request.  It contains no plan or evidence blob and grants no
//! cleanup, capacity-release, filesystem, or source-database authority.  The owner must later
//! establish those outcomes before any cancellation terminal transition can be considered.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::admission_claims::{AdmissionClaimInventory, AdmissionClaimValidationError};
use super::model::OperationId;
use super::work_plan::{
    WorkPlan, WorkPlanError, WorkUnit, WorkUnitApplicability, WorkUnitKey, WorkUnitRole,
};
use super::work_progress::{ProgressStepStatus, WorkProgressStep};

/// Maximum number of action snapshots retained by one cancellation inventory.
pub const MAX_CANCELLATION_ACTIONS: usize = 256;
/// Maximum number of keyed step snapshots retained by one cancellation inventory.
pub const MAX_CANCELLATION_STEPS: usize = super::work_plan::MAX_WORK_PLAN_UNITS;

/// Route selected from observed action effects at cancellation request time.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum CancellationRoute {
    /// No application effect was observed before cancellation.
    BeforePublish,
    /// A normal application publication or qualified relocation was observed.
    AfterPublish,
    /// A non-publication application, destructive, or external effect was observed.
    AfterObservedEffect,
}

/// Derived state of one action at cancellation request time.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum CancellationActionState {
    /// No preparation or effect completion is recorded for the action.
    ///
    /// This does not prove that an eligible first effect is not already in flight between
    /// checkpoints; the guarded progress layer must retain that boundary as nonterminal.
    NotStarted,
    /// Preparation, staging, or recovery material work has begun without an observed effect.
    CleanablePrePublish,
    /// A retained-merge application mutation completed without publication.
    ApplicationMutationWithoutPublish,
    /// A selected retained object disposition or its absence was observed without publication.
    DestructiveDispositionWithoutPublish,
    /// A normal publication or qualified relocation completed.
    AppPublished,
    /// An external audit completed; source reconciliation status is retained explicitly.
    ExternalEffectAudited {
        /// Whether the action's source-affect obligation was also reconciled.
        source_reconciled: bool,
    },
}

/// One action ordinal and its frozen cancellation state.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CancellationActionSnapshot {
    action_ordinal: u8,
    state: CancellationActionState,
}

impl CancellationActionSnapshot {
    /// Return the captured action ordinal.
    pub const fn action_ordinal(self) -> u8 {
        self.action_ordinal
    }

    /// Return the derived action state.
    pub const fn state(self) -> CancellationActionState {
        self.state
    }
}

/// One exact work-plan key and status captured when cancellation was requested.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CancellationStepSnapshot {
    key: WorkUnitKey,
    status: ProgressStepStatus,
}

impl CancellationStepSnapshot {
    /// Return the frozen work-plan key.
    pub const fn key(self) -> WorkUnitKey {
        self.key
    }

    /// Return the frozen progress status.
    pub const fn status(self) -> ProgressStepStatus {
        self.status
    }
}

/// Immutable bounded cancellation inventory retained by full progress.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CancellationInventory {
    operation_id: OperationId,
    plan_version: u16,
    execution_epoch: u64,
    generation: u64,
    steps: Vec<CancellationStepSnapshot>,
    actions: Vec<CancellationActionSnapshot>,
    route: CancellationRoute,
    admission_claims: AdmissionClaimInventory,
}

impl CancellationInventory {
    /// Capture one exact progress state after validating all bounded inputs.
    pub(crate) fn capture(
        operation_id: OperationId,
        plan: &WorkPlan,
        execution_epoch: u64,
        generation: u64,
        progress_steps: &[WorkProgressStep],
        admission_claims: &AdmissionClaimInventory,
    ) -> Result<Self, CancellationInventoryError> {
        let action_count = validate_capture_inputs(
            operation_id,
            plan,
            generation,
            progress_steps,
            admission_claims,
        )?;

        let mut steps = Vec::with_capacity(progress_steps.len());
        for step in progress_steps {
            steps.push(CancellationStepSnapshot {
                key: step.key(),
                status: step.status(),
            });
        }

        let mut actions = Vec::with_capacity(action_count);
        let mut start = 0;
        while start < plan.units().len() {
            let action_ordinal = plan.units()[start].key().action_ordinal();
            let mut end = start + 1;
            while end < plan.units().len()
                && plan.units()[end].key().action_ordinal() == action_ordinal
            {
                end += 1;
            }
            actions.push(CancellationActionSnapshot {
                action_ordinal,
                state: derive_action_state(plan, start, end, |index| {
                    progress_steps[index].status()
                }),
            });
            start = end;
        }

        let route = route_for_actions(&actions);
        Ok(Self {
            operation_id,
            plan_version: plan.plan_version(),
            execution_epoch,
            generation,
            steps,
            actions,
            route,
            admission_claims: admission_claims.clone(),
        })
    }

    /// Validate a decoded or retained inventory against the exact frozen plan and claim set.
    pub fn validate_against(
        &self,
        operation_id: OperationId,
        plan: &WorkPlan,
        current_steps: &[WorkProgressStep],
        execution_epoch: u64,
        admission_claims: &AdmissionClaimInventory,
    ) -> Result<(), CancellationInventoryError> {
        if operation_id.as_uuid().is_nil() {
            return Err(CancellationInventoryError::InvalidOperationId);
        }
        if self.operation_id != operation_id {
            return Err(CancellationInventoryError::OperationIdMismatch);
        }
        if self.plan_version != plan.plan_version() {
            return Err(CancellationInventoryError::PlanVersionMismatch {
                expected: plan.plan_version(),
                actual: self.plan_version,
            });
        }
        if self.generation == 0 {
            return Err(CancellationInventoryError::InvalidGeneration);
        }
        if self.execution_epoch > execution_epoch {
            return Err(CancellationInventoryError::ExecutionEpochFuture {
                current: execution_epoch,
                actual: self.execution_epoch,
            });
        }
        self.admission_claims.validate()?;
        admission_claims.validate()?;
        if self.admission_claims != *admission_claims {
            return Err(CancellationInventoryError::AdmissionClaimsMismatch);
        }
        validate_plan_shape(plan)?;
        let action_count = validate_snapshot_steps(plan, &self.steps)?;
        validate_snapshot_prerequisites(plan, &self.steps)?;
        validate_progress_step_shape(plan, current_steps)?;
        for (index, (snapshot, current)) in self.steps.iter().zip(current_steps).enumerate() {
            if snapshot.key != current.key() || snapshot.status != current.status() {
                return Err(CancellationInventoryError::CurrentProgressMismatch { index });
            }
        }
        if self.actions.len() != action_count {
            return Err(CancellationInventoryError::ActionCountMismatch {
                expected: action_count,
                actual: self.actions.len(),
            });
        }

        let mut action_index = 0;
        let mut start = 0;
        while start < plan.units().len() {
            let action_ordinal = plan.units()[start].key().action_ordinal();
            let mut end = start + 1;
            while end < plan.units().len()
                && plan.units()[end].key().action_ordinal() == action_ordinal
            {
                end += 1;
            }
            let snapshot = self.actions[action_index];
            if snapshot.action_ordinal != action_ordinal {
                return Err(CancellationInventoryError::ActionOrdinalMismatch {
                    index: action_index,
                    expected: action_ordinal,
                    actual: snapshot.action_ordinal,
                });
            }
            let expected_state =
                derive_action_state(plan, start, end, |index| self.steps[index].status);
            if snapshot.state != expected_state {
                return Err(CancellationInventoryError::ActionStateMismatch {
                    action_ordinal,
                    expected: expected_state,
                    actual: snapshot.state,
                });
            }
            action_index += 1;
            start = end;
        }
        let expected_route = route_for_actions(&self.actions);
        if self.route != expected_route {
            return Err(CancellationInventoryError::RouteMismatch {
                expected: expected_route,
                actual: self.route,
            });
        }
        Ok(())
    }

    /// Return the operation identity frozen into this inventory.
    pub const fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    /// Return the frozen work-plan derivation version.
    pub const fn plan_version(&self) -> u16 {
        self.plan_version
    }

    /// Return the execution epoch at cancellation request time.
    pub const fn execution_epoch(&self) -> u64 {
        self.execution_epoch
    }

    /// Return the one-based cancellation generation.
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Borrow the exact ordered keyed progress snapshot.
    pub fn steps(&self) -> &[CancellationStepSnapshot] {
        &self.steps
    }

    /// Borrow all derived action states in action-ordinal order.
    pub fn actions(&self) -> &[CancellationActionSnapshot] {
        &self.actions
    }

    /// Return the route selected from the observed action effects.
    pub const fn route(&self) -> CancellationRoute {
        self.route
    }

    /// Borrow the exact immutable admission-claim observation frozen with cancellation.
    pub fn admission_claims(&self) -> &AdmissionClaimInventory {
        &self.admission_claims
    }
}

fn validate_capture_inputs(
    operation_id: OperationId,
    plan: &WorkPlan,
    generation: u64,
    progress_steps: &[WorkProgressStep],
    admission_claims: &AdmissionClaimInventory,
) -> Result<usize, CancellationInventoryError> {
    if operation_id.as_uuid().is_nil() {
        return Err(CancellationInventoryError::InvalidOperationId);
    }
    if generation == 0 {
        return Err(CancellationInventoryError::InvalidGeneration);
    }
    validate_plan_shape(plan)?;
    admission_claims.validate()?;
    if progress_steps.len() != plan.units().len() {
        return Err(CancellationInventoryError::StepCountMismatch {
            expected: plan.units().len(),
            actual: progress_steps.len(),
        });
    }
    for (index, (unit, step)) in plan.units().iter().zip(progress_steps).enumerate() {
        if unit.key() != step.key() {
            return Err(CancellationInventoryError::StepKeyMismatch {
                index,
                expected: unit.key(),
                actual: step.key(),
            });
        }
        validate_step_status(unit, step.status())?;
    }
    validate_progress_prerequisites(plan, progress_steps)?;
    Ok(action_count(plan))
}

fn validate_plan_shape(plan: &WorkPlan) -> Result<(), CancellationInventoryError> {
    plan.validate()?;
    if plan.units().len() > MAX_CANCELLATION_STEPS {
        return Err(CancellationInventoryError::TooManySteps {
            actual: plan.units().len(),
            maximum: MAX_CANCELLATION_STEPS,
        });
    }
    let actions = action_count(plan);
    if actions > MAX_CANCELLATION_ACTIONS {
        return Err(CancellationInventoryError::TooManyActions {
            actual: actions,
            maximum: MAX_CANCELLATION_ACTIONS,
        });
    }
    Ok(())
}

fn validate_snapshot_steps(
    plan: &WorkPlan,
    steps: &[CancellationStepSnapshot],
) -> Result<usize, CancellationInventoryError> {
    if steps.len() > MAX_CANCELLATION_STEPS {
        return Err(CancellationInventoryError::TooManySteps {
            actual: steps.len(),
            maximum: MAX_CANCELLATION_STEPS,
        });
    }
    if steps.len() != plan.units().len() {
        return Err(CancellationInventoryError::StepCountMismatch {
            expected: plan.units().len(),
            actual: steps.len(),
        });
    }
    for (index, (unit, step)) in plan.units().iter().zip(steps).enumerate() {
        if unit.key() != step.key {
            return Err(CancellationInventoryError::StepKeyMismatch {
                index,
                expected: unit.key(),
                actual: step.key,
            });
        }
        validate_step_status(unit, step.status)?;
    }
    Ok(action_count(plan))
}

fn validate_progress_prerequisites(
    plan: &WorkPlan,
    steps: &[WorkProgressStep],
) -> Result<(), CancellationInventoryError> {
    for edge in plan.prerequisites() {
        let dependent_index = plan
            .units()
            .binary_search_by_key(&edge.dependent(), |unit| unit.key())
            .map_err(|_| CancellationInventoryError::PrerequisiteMissing { edge: *edge })?;
        if steps[dependent_index].status() != ProgressStepStatus::Completed {
            continue;
        }
        let prerequisite_index = plan
            .units()
            .binary_search_by_key(&edge.prerequisite(), |unit| unit.key())
            .map_err(|_| CancellationInventoryError::PrerequisiteMissing { edge: *edge })?;
        let prerequisite_status = steps[prerequisite_index].status();
        if prerequisite_status != ProgressStepStatus::Completed
            && prerequisite_status != ProgressStepStatus::NotApplicable
        {
            return Err(CancellationInventoryError::PrerequisiteIncomplete { edge: *edge });
        }
    }
    Ok(())
}

fn validate_progress_step_shape(
    plan: &WorkPlan,
    steps: &[WorkProgressStep],
) -> Result<(), CancellationInventoryError> {
    if steps.len() != plan.units().len() {
        return Err(CancellationInventoryError::StepCountMismatch {
            expected: plan.units().len(),
            actual: steps.len(),
        });
    }
    for (index, (unit, step)) in plan.units().iter().zip(steps).enumerate() {
        if unit.key() != step.key() {
            return Err(CancellationInventoryError::CurrentProgressKeyMismatch {
                index,
                expected: unit.key(),
                actual: step.key(),
            });
        }
        validate_step_status(unit, step.status())?;
    }
    Ok(())
}

fn validate_snapshot_prerequisites(
    plan: &WorkPlan,
    steps: &[CancellationStepSnapshot],
) -> Result<(), CancellationInventoryError> {
    for edge in plan.prerequisites() {
        let dependent_index = plan
            .units()
            .binary_search_by_key(&edge.dependent(), |unit| unit.key())
            .map_err(|_| CancellationInventoryError::PrerequisiteMissing { edge: *edge })?;
        if steps[dependent_index].status != ProgressStepStatus::Completed {
            continue;
        }
        let prerequisite_index = plan
            .units()
            .binary_search_by_key(&edge.prerequisite(), |unit| unit.key())
            .map_err(|_| CancellationInventoryError::PrerequisiteMissing { edge: *edge })?;
        let prerequisite_status = steps[prerequisite_index].status;
        if prerequisite_status != ProgressStepStatus::Completed
            && prerequisite_status != ProgressStepStatus::NotApplicable
        {
            return Err(CancellationInventoryError::PrerequisiteIncomplete { edge: *edge });
        }
    }
    Ok(())
}

fn validate_step_status(
    unit: &WorkUnit,
    status: ProgressStepStatus,
) -> Result<(), CancellationInventoryError> {
    if unit.applicability() == WorkUnitApplicability::NotApplicable {
        if status != ProgressStepStatus::NotApplicable {
            return Err(CancellationInventoryError::InvalidStepStatus {
                key: unit.key(),
                status,
            });
        }
        return Ok(());
    }
    if status == ProgressStepStatus::NotApplicable {
        return Err(CancellationInventoryError::InvalidStepStatus {
            key: unit.key(),
            status,
        });
    }
    if status == ProgressStepStatus::Deferred
        && unit.requirement() != super::participants::ParticipantRequirement::OptionalRebuildable
    {
        return Err(CancellationInventoryError::InvalidStepStatus {
            key: unit.key(),
            status,
        });
    }
    Ok(())
}

fn action_count(plan: &WorkPlan) -> usize {
    let mut count = 0;
    let mut previous = None;
    for unit in plan.units() {
        let action = unit.key().action_ordinal();
        if previous != Some(action) {
            count += 1;
            previous = Some(action);
        }
    }
    count
}

fn derive_action_state<F>(
    plan: &WorkPlan,
    start: usize,
    end: usize,
    status: F,
) -> CancellationActionState
where
    F: Fn(usize) -> ProgressStepStatus,
{
    let mut app_published = false;
    let mut external_audited = false;
    let mut source_reconciled = false;
    let mut application_mutation = false;
    let mut destructive_disposition = false;
    let mut cleanable_pre_publish = false;

    for index in start..end {
        if status(index) != ProgressStepStatus::Completed {
            continue;
        }
        match plan.units()[index].key().role() {
            WorkUnitRole::Publish | WorkUnitRole::QualifiedRelocation => {
                app_published = true;
            }
            WorkUnitRole::ExternalAudit => {
                external_audited = true;
            }
            WorkUnitRole::SourceAffect => {
                source_reconciled = true;
            }
            WorkUnitRole::RetainedMerge => {
                application_mutation = true;
            }
            WorkUnitRole::SelectedRetainedObjectDisposition
            | WorkUnitRole::RetainedObjectAbsence => {
                destructive_disposition = true;
            }
            WorkUnitRole::Prepare
            | WorkUnitRole::RecoveryMaterialRetained
            | WorkUnitRole::RestoreRecordPrepared
            | WorkUnitRole::EnsureContainedParentDirectories
            | WorkUnitRole::Stage
            | WorkUnitRole::CopyValidated
            | WorkUnitRole::OriginRemovalStart
            | WorkUnitRole::OriginAbsence => {
                cleanable_pre_publish = true;
            }
            _ => {}
        }
    }

    if app_published {
        CancellationActionState::AppPublished
    } else if external_audited {
        CancellationActionState::ExternalEffectAudited { source_reconciled }
    } else if application_mutation {
        CancellationActionState::ApplicationMutationWithoutPublish
    } else if destructive_disposition {
        CancellationActionState::DestructiveDispositionWithoutPublish
    } else if cleanable_pre_publish {
        CancellationActionState::CleanablePrePublish
    } else {
        CancellationActionState::NotStarted
    }
}

fn route_for_actions(actions: &[CancellationActionSnapshot]) -> CancellationRoute {
    if actions
        .iter()
        .any(|action| action.state == CancellationActionState::AppPublished)
    {
        CancellationRoute::AfterPublish
    } else if actions.iter().any(|action| {
        matches!(
            action.state,
            CancellationActionState::ApplicationMutationWithoutPublish
                | CancellationActionState::DestructiveDispositionWithoutPublish
                | CancellationActionState::ExternalEffectAudited { .. }
        )
    }) {
        CancellationRoute::AfterObservedEffect
    } else {
        CancellationRoute::BeforePublish
    }
}

/// Failure while capturing or validating a bounded cancellation inventory.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum CancellationInventoryError {
    /// The operation identity was nil.
    #[error("cancellation inventory operation identity must be non-nil")]
    InvalidOperationId,
    /// The captured operation identity differed from the current operation.
    #[error("cancellation inventory operation identity does not match")]
    OperationIdMismatch,
    /// The frozen plan derivation version differed from the current plan.
    #[error("cancellation inventory plan version {actual} does not match expected {expected}")]
    PlanVersionMismatch {
        /// Plan version required by the current frozen plan.
        expected: u16,
        /// Plan version retained by the cancellation inventory.
        actual: u16,
    },
    /// The captured execution epoch was newer than the current epoch.
    #[error("cancellation inventory execution epoch {actual} is newer than current {current}")]
    ExecutionEpochFuture {
        /// Current execution epoch of progress.
        current: u64,
        /// Execution epoch retained by the cancellation inventory.
        actual: u64,
    },
    /// The cancellation generation was zero and therefore not a first-generation observation.
    #[error("cancellation inventory generation must be non-zero")]
    InvalidGeneration,
    /// The frozen plan itself was invalid.
    #[error("invalid cancellation inventory plan: {0}")]
    Plan(#[from] WorkPlanError),
    /// The exact progress snapshot had a different number of steps.
    #[error("cancellation inventory has {actual} steps, expected {expected}")]
    StepCountMismatch {
        /// Number of plan units that must be represented.
        expected: usize,
        /// Number of snapshot steps supplied.
        actual: usize,
    },
    /// One snapshot key differed from the exact frozen plan order.
    #[error(
        "cancellation inventory step {index} key {actual:?} differs from expected {expected:?}"
    )]
    StepKeyMismatch {
        /// Position in the frozen plan order.
        index: usize,
        /// Key required by the frozen plan.
        expected: WorkUnitKey,
        /// Key retained by the cancellation snapshot.
        actual: WorkUnitKey,
    },
    /// A progress status did not match the plan's applicability.
    #[error("cancellation inventory step {key:?} has invalid status {status:?}")]
    InvalidStepStatus {
        /// Key whose status did not match applicability.
        key: WorkUnitKey,
        /// Status that failed applicability validation.
        status: ProgressStepStatus,
    },
    /// The plan exceeded the bounded action snapshot limit.
    #[error("cancellation inventory has {actual} actions, maximum is {maximum}")]
    TooManyActions {
        /// Number of action ordinals observed.
        actual: usize,
        /// Maximum action ordinals permitted.
        maximum: usize,
    },
    /// The plan exceeded the bounded keyed snapshot limit.
    #[error("cancellation inventory has {actual} steps, maximum is {maximum}")]
    TooManySteps {
        /// Number of keyed steps observed.
        actual: usize,
        /// Maximum keyed steps permitted.
        maximum: usize,
    },
    /// The action snapshot count did not cover each plan action exactly once.
    #[error("cancellation inventory has {actual} action snapshots, expected {expected}")]
    ActionCountMismatch {
        /// Number of action ordinals in the frozen plan.
        expected: usize,
        /// Number of action snapshots retained.
        actual: usize,
    },
    /// An action snapshot was not in exact action-ordinal order.
    #[error(
        "cancellation inventory action snapshot {index} ordinal {actual} differs from expected {expected}"
    )]
    ActionOrdinalMismatch {
        /// Position in action-ordinal order.
        index: usize,
        /// Action ordinal required by the frozen plan.
        expected: u8,
        /// Action ordinal retained by the cancellation inventory.
        actual: u8,
    },
    /// An action snapshot state did not match its keyed progress statuses.
    #[error(
        "cancellation inventory action {action_ordinal} state {actual:?} differs from expected {expected:?}"
    )]
    ActionStateMismatch {
        /// Action ordinal whose derived state differed.
        action_ordinal: u8,
        /// State derived from the exact step snapshot.
        expected: CancellationActionState,
        /// State retained by the cancellation inventory.
        actual: CancellationActionState,
    },
    /// The current progress snapshot differs from the frozen cancellation inventory.
    #[error("cancellation inventory differs from current progress at step {index}")]
    CurrentProgressMismatch {
        /// Position in the frozen plan order that changed.
        index: usize,
    },
    /// Current progress did not retain the exact frozen plan key order.
    #[error("current progress step {index} key {actual:?} differs from expected {expected:?}")]
    CurrentProgressKeyMismatch {
        /// Position in the frozen plan order.
        index: usize,
        /// Key required by the frozen plan.
        expected: WorkUnitKey,
        /// Key retained by current progress.
        actual: WorkUnitKey,
    },
    /// A completed step did not have its exact prerequisite completion.
    #[error("cancellation inventory completed step has incomplete prerequisite {edge:?}")]
    PrerequisiteIncomplete {
        /// Prerequisite edge whose dependent status was inconsistent.
        edge: super::work_plan::WorkPrerequisite,
    },
    /// A prerequisite endpoint was absent while validating the frozen plan.
    #[error("cancellation inventory prerequisite endpoint is missing: {edge:?}")]
    PrerequisiteMissing {
        /// Edge carrying the missing endpoint.
        edge: super::work_plan::WorkPrerequisite,
    },
    /// The stored route did not match the derived action states.
    #[error("cancellation inventory route {actual:?} differs from expected {expected:?}")]
    RouteMismatch {
        /// Route derived from the retained action states.
        expected: CancellationRoute,
        /// Route retained by the cancellation inventory.
        actual: CancellationRoute,
    },
    /// The retained claim observation had changed since cancellation was captured.
    #[error("cancellation inventory admission claims do not match")]
    AdmissionClaimsMismatch,
    /// The retained claim observation was malformed.
    #[error("invalid cancellation inventory admission claims: {0}")]
    AdmissionClaims(#[from] AdmissionClaimValidationError),
}

#[cfg(test)]
mod tests {
    use super::{CancellationActionState, CancellationRoute};

    #[test]
    fn observed_effect_routes_are_distinct_from_publication() {
        assert_ne!(
            CancellationRoute::AfterObservedEffect,
            CancellationRoute::AfterPublish
        );
        assert_eq!(
            CancellationActionState::ExternalEffectAudited {
                source_reconciled: false
            },
            CancellationActionState::ExternalEffectAudited {
                source_reconciled: false
            }
        );
    }
}
