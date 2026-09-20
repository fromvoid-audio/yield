//! Bounded, non-authoritative observations of the complete in-memory progress value.
//!
//! This module captures the exact mutable progress observations together with the immutable
//! intent retained by the caller.  The base validator checks topology, step shape, historical
//! completion evidence, direct prerequisite consistency, and the bounded internal shape of retry,
//! audit, cancellation envelopes, and retained after-effect reconciliation observations.  It
//! intentionally does not validate advanced cancellation, recovery, claim-settlement, cleanup,
//! or terminal semantics; a snapshot is not a durable schema,
//! a restart record, or a progress authority. The production owner must retain this immutable intent
//! from admission because capture cannot recover historical artifact provenance from the plan-only live progress
//! value.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::super::full_progress_record::{
    FullProgressIntentObservation, FullProgressIntentObservationError,
};
use super::super::participants::ParticipantValidationError;
use super::super::readiness_scheduling::ReadinessDeferralBatchObservation;
use super::super::work_plan::{WorkUnitApplicability, WorkUnitKey, WorkUnitRole};
use super::{
    ActiveAudit, AdmissionClaimSettlementInventory, AdmissionClaimValidationError,
    AfterEffectReconciliationObservation, AuditTargetValidationError,
    CancellationCleanupObservation, CancellationDispatchOutcomeRecordError, CancellationInventory,
    CancellationInventoryError, CancellationRequestObservation, CancellationRequestValidationError,
    CancellationRoute, FullProgressDisposition, OperationPhase, ParticipantRequirement,
    ParticipantScope, PrePublishCancellationObservation, ProgressStepStatus, RetryObservation,
    RetryObservationValidationError, WorkProgress, WorkProgressError, WorkProgressStep,
    historical_scope_matches_current, same_stable_source_identity,
};

/// A complete bounded in-memory progress observation tied to one frozen intent.
///
/// The immutable operation identity, work plan, admission claims, and origin policies are kept
/// only in [`FullProgressIntentObservation`].  The remaining fields are the exact mutable
/// observations retained by [`WorkProgress`].  This value does not establish live capacity,
/// ownership, recovery, persistence, or restart authority.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FullProgressSnapshotObservation {
    intent: FullProgressIntentObservation,
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
    claim_settlement: Option<AdmissionClaimSettlementInventory>,
    cancellation_inventory: Option<CancellationInventory>,
    recovery_generation: u64,
    pre_publish_cancellation: Option<PrePublishCancellationObservation>,
    after_effect_reconciliation: Option<AfterEffectReconciliationObservation>,
    cancellation_cleanup_observations: Option<CancellationCleanupObservation>,
    readiness_deferral_batches: Vec<ReadinessDeferralBatchObservation>,
}

impl FullProgressSnapshotObservation {
    /// Borrow the exact frozen intent retained with this snapshot.
    pub fn intent(&self) -> &FullProgressIntentObservation {
        &self.intent
    }

    /// Borrow current participant scopes in their retained order.
    pub fn scopes(&self) -> &[ParticipantScope] {
        &self.scopes
    }

    /// Borrow exact work-plan progress rows in frozen plan order.
    pub fn steps(&self) -> &[WorkProgressStep] {
        &self.steps
    }

    /// Return the observed progress phase.
    pub const fn phase(&self) -> OperationPhase {
        self.phase
    }

    /// Return the observed guarded disposition.
    pub const fn disposition(&self) -> FullProgressDisposition {
        self.disposition
    }

    /// Borrow the retained retry observation, when present.
    pub fn retry_observation(&self) -> Option<&RetryObservation> {
        self.retry_observation.as_ref()
    }

    /// Borrow the most recent retry observation, when present.
    pub fn last_retry_observation(&self) -> Option<&RetryObservation> {
        self.last_retry_observation.as_ref()
    }

    /// Return the observed execution epoch.
    pub const fn execution_epoch(&self) -> u64 {
        self.execution_epoch
    }

    /// Borrow the active audit observation, when present.
    pub fn audit_observation(&self) -> Option<&super::ProgressAuditObservation> {
        self.active_audit.as_ref().map(|audit| &audit.observation)
    }

    /// Return the retained audit generation.
    pub const fn audit_generation(&self) -> u64 {
        self.audit_generation
    }

    /// Return the retained cancellation request, when present.
    pub const fn cancellation_request(&self) -> Option<CancellationRequestObservation> {
        self.cancellation_request
    }

    /// Borrow the retained claim settlement observation, when present.
    pub fn claim_settlement(&self) -> Option<&AdmissionClaimSettlementInventory> {
        self.claim_settlement.as_ref()
    }

    /// Borrow the frozen cancellation inventory, when present.
    pub fn cancellation_inventory(&self) -> Option<&CancellationInventory> {
        self.cancellation_inventory.as_ref()
    }

    /// Return the observed cancellation recovery generation.
    pub const fn recovery_generation(&self) -> u64 {
        self.recovery_generation
    }

    /// Borrow retained prepublication cancellation observations, when present.
    pub fn pre_publish_cancellation(&self) -> Option<&PrePublishCancellationObservation> {
        self.pre_publish_cancellation.as_ref()
    }

    /// Borrow retained after-effect reconciliation observations, when present.
    pub fn after_effect_reconciliation(&self) -> Option<&AfterEffectReconciliationObservation> {
        self.after_effect_reconciliation.as_ref()
    }

    /// Borrow retained resource and namespace cleanup observations, when present.
    pub fn cancellation_cleanup_observations(&self) -> Option<&CancellationCleanupObservation> {
        self.cancellation_cleanup_observations.as_ref()
    }

    /// Borrow compact accepted optional-artifact deferral history.
    pub fn readiness_deferral_batches(&self) -> &[ReadinessDeferralBatchObservation] {
        &self.readiness_deferral_batches
    }

    /// Validate this snapshot's topology and retained completion history without live progress.
    ///
    /// This is deliberately a partial consistency check.  It proves that the supplied intent,
    /// current scopes, frozen plan rows, retained completion history, direct prerequisite
    /// relationships, the internal retry/audit observation shape, and the bounded cancellation
    /// envelope agree with one another, while preserving advanced state-family observations
    /// without claiming to validate their recovery or terminal semantics.
    pub(crate) fn validate_base(&self) -> Result<(), FullProgressSnapshotObservationError> {
        self.intent
            .validate()
            .map_err(FullProgressSnapshotObservationError::Intent)?;
        self.validate_scopes()?;
        self.validate_step_bounds()?;
        self.validate_readiness_deferral_bounds()?;
        self.validate_retry_state()?;
        self.validate_audit_state()?;
        self.validate_cancellation_envelope()?;
        let validation_progress = self.validation_progress();
        self.validate_steps(&validation_progress)?;
        validation_progress
            .validate_readiness_deferral_history()
            .map_err(|source| {
                FullProgressSnapshotObservationError::ReadinessDeferralValidation { source }
            })?;
        self.validate_after_effect_reconciliation(&validation_progress)?;
        self.validate_cancellation_cleanup_observations(&validation_progress)?;
        self.validate_pre_publish_cancellation(&validation_progress)?;
        self.validate_lifecycle_consistency(&validation_progress)?;
        Ok(())
    }

    fn validate_readiness_deferral_bounds(
        &self,
    ) -> Result<(), FullProgressSnapshotObservationError> {
        let maximum = self.intent.plan().units().len();
        if self.readiness_deferral_batches.len() > maximum {
            return Err(
                FullProgressSnapshotObservationError::TooManyReadinessDeferralBatches {
                    actual: self.readiness_deferral_batches.len(),
                    maximum,
                },
            );
        }
        let mut keys = 0usize;
        let mut rows = 0usize;
        for batch in &self.readiness_deferral_batches {
            keys = keys.saturating_add(batch.artifact_keys().len());
            rows = rows.saturating_add(batch.rows().len());
            if keys > maximum || rows > maximum {
                return Err(
                    FullProgressSnapshotObservationError::TooManyReadinessDeferralObservations {
                        keys,
                        rows,
                        maximum,
                    },
                );
            }
        }
        Ok(())
    }

    fn validate_after_effect_reconciliation(
        &self,
        progress: &WorkProgress,
    ) -> Result<(), FullProgressSnapshotObservationError> {
        let Some(accepted) = self.after_effect_reconciliation.as_ref() else {
            return Ok(());
        };
        if accepted.execution_epoch() > self.execution_epoch {
            return Err(
                FullProgressSnapshotObservationError::AfterEffectExecutionEpochFuture {
                    accepted: accepted.execution_epoch(),
                    current: self.execution_epoch,
                },
            );
        }

        let recovery = accepted.recovery_inventory();
        let dispatch = recovery.dispatch_outcome();
        let source = accepted.source_closure_observation();
        let coordinator = accepted.coordinator_closure_observation();
        let accepted_epoch = accepted.execution_epoch();
        if dispatch.observation_epoch() != accepted_epoch {
            return Err(
                FullProgressSnapshotObservationError::AfterEffectDispatchEpochMismatch {
                    accepted: accepted_epoch,
                    actual: dispatch.observation_epoch(),
                },
            );
        }
        if source.observation_epoch() != accepted_epoch {
            return Err(
                FullProgressSnapshotObservationError::AfterEffectSourceClosureEpochMismatch {
                    accepted: accepted_epoch,
                    actual: source.observation_epoch(),
                },
            );
        }
        if coordinator.observation_epoch() != accepted_epoch {
            return Err(
                FullProgressSnapshotObservationError::AfterEffectCoordinatorClosureEpochMismatch {
                    accepted: accepted_epoch,
                    actual: coordinator.observation_epoch(),
                },
            );
        }
        if accepted.recovery_generation() == 0 {
            return Err(FullProgressSnapshotObservationError::AfterEffectGenerationZero);
        }
        if accepted.recovery_generation() > self.recovery_generation {
            return Err(
                FullProgressSnapshotObservationError::AfterEffectGenerationFuture {
                    accepted: accepted.recovery_generation(),
                    current: self.recovery_generation,
                },
            );
        }
        if dispatch.recovery_generation() == 0 {
            return Err(FullProgressSnapshotObservationError::AfterEffectDispatchGenerationZero);
        }
        if dispatch.recovery_generation() > accepted.recovery_generation() {
            return Err(
                FullProgressSnapshotObservationError::AfterEffectDispatchGenerationFuture {
                    accepted: accepted.recovery_generation(),
                    actual: dispatch.recovery_generation(),
                },
            );
        }

        progress
            .validate_cancellation_effect_recovery_inventory(recovery, dispatch)
            .map_err(
                |source| FullProgressSnapshotObservationError::AfterEffectValidation { source },
            )?;
        progress
            .validate_cancellation_source_closure_observation_record(source, recovery, dispatch)
            .map_err(
                |source| FullProgressSnapshotObservationError::AfterEffectValidation { source },
            )?;
        progress
            .validate_cancellation_coordinator_closure_observation_record(
                coordinator,
                source,
                recovery,
                dispatch,
            )
            .map_err(
                |source| FullProgressSnapshotObservationError::AfterEffectValidation { source },
            )?;
        super::validate_after_effect_phase_gates(dispatch, source, coordinator).map_err(
            |source| FullProgressSnapshotObservationError::AfterEffectValidation { source },
        )?;

        if let Some(previous_dispatch) = self
            .pre_publish_cancellation
            .as_ref()
            .and_then(PrePublishCancellationObservation::dispatch_outcome)
        {
            dispatch
                .validate_transition(previous_dispatch, self.execution_epoch)
                .map_err(|source| {
                    FullProgressSnapshotObservationError::AfterEffectDispatchTransition { source }
                })?;
            if accepted.recovery_generation() <= previous_dispatch.recovery_generation() {
                return Err(
                    FullProgressSnapshotObservationError::AfterEffectGenerationNotAfterPrePublish {
                        accepted: accepted.recovery_generation(),
                        previous: previous_dispatch.recovery_generation(),
                    },
                );
            }
        }
        Ok(())
    }

    fn validate_cancellation_cleanup_observations(
        &self,
        progress: &WorkProgress,
    ) -> Result<(), FullProgressSnapshotObservationError> {
        let Some(cleanup) = self.cancellation_cleanup_observations.as_ref() else {
            return Ok(());
        };
        let Some(after_effect) = self.after_effect_reconciliation.as_ref() else {
            return Err(
                FullProgressSnapshotObservationError::CancellationCleanupAfterEffectMissing,
            );
        };

        let resource = cleanup.resource_cleanup_observation();
        let namespace = cleanup.namespace_cleanup_observation();
        let cleanup_manifest = resource.manifest().cleanup_manifest();
        let cleanup_recovery = cleanup_manifest.recovery_inventory();
        let cleanup_dispatch = cleanup_manifest.dispatch_outcome();
        let cleanup_epoch = cleanup.execution_epoch();
        let cleanup_generation = cleanup.recovery_generation();
        let linked_after_effect_generation = cleanup.after_effect_generation();

        if cleanup_epoch > self.execution_epoch {
            return Err(
                FullProgressSnapshotObservationError::CancellationCleanupExecutionEpochFuture {
                    cleanup: cleanup_epoch,
                    current: self.execution_epoch,
                },
            );
        }
        if resource.observation_epoch() != cleanup_epoch {
            return Err(
                FullProgressSnapshotObservationError::CancellationCleanupResourceEpochMismatch {
                    cleanup: cleanup_epoch,
                    actual: resource.observation_epoch(),
                },
            );
        }
        if namespace.observation_epoch() != cleanup_epoch {
            return Err(
                FullProgressSnapshotObservationError::CancellationCleanupNamespaceEpochMismatch {
                    cleanup: cleanup_epoch,
                    actual: namespace.observation_epoch(),
                },
            );
        }
        if cleanup_dispatch.observation_epoch() != cleanup_epoch {
            return Err(
                FullProgressSnapshotObservationError::CancellationCleanupDispatchEpochMismatch {
                    cleanup: cleanup_epoch,
                    actual: cleanup_dispatch.observation_epoch(),
                },
            );
        }
        if linked_after_effect_generation == 0 {
            return Err(
                FullProgressSnapshotObservationError::CancellationCleanupAfterEffectGenerationZero,
            );
        }
        if cleanup_generation == 0 {
            return Err(FullProgressSnapshotObservationError::CancellationCleanupGenerationZero);
        }
        if linked_after_effect_generation >= cleanup_generation {
            return Err(
                FullProgressSnapshotObservationError::CancellationCleanupAfterEffectGenerationNotBeforeCleanup {
                    after_effect: linked_after_effect_generation,
                    cleanup: cleanup_generation,
                },
            );
        }
        if cleanup_generation > self.recovery_generation {
            return Err(
                FullProgressSnapshotObservationError::CancellationCleanupGenerationFuture {
                    cleanup: cleanup_generation,
                    current: self.recovery_generation,
                },
            );
        }
        if cleanup_dispatch.recovery_generation() > linked_after_effect_generation {
            return Err(
                FullProgressSnapshotObservationError::CancellationCleanupDispatchGenerationFuture {
                    dispatch: cleanup_dispatch.recovery_generation(),
                    after_effect: linked_after_effect_generation,
                },
            );
        }
        if linked_after_effect_generation > after_effect.recovery_generation() {
            return Err(
                FullProgressSnapshotObservationError::CancellationCleanupAfterEffectGenerationFuture {
                    cleanup: linked_after_effect_generation,
                    current: after_effect.recovery_generation(),
                },
            );
        }

        progress
            .validate_cancellation_effect_recovery_inventory(cleanup_recovery, cleanup_dispatch)
            .map_err(|source| {
                FullProgressSnapshotObservationError::CancellationCleanupValidation { source }
            })?;
        progress
            .validate_cancellation_cleanup_manifest(
                cleanup_manifest,
                cleanup_recovery,
                cleanup_dispatch,
            )
            .map_err(|source| {
                FullProgressSnapshotObservationError::CancellationCleanupValidation { source }
            })?;
        progress
            .validate_cancellation_resource_cleanup_observation_record(
                resource,
                cleanup_manifest,
                cleanup_recovery,
                cleanup_dispatch,
            )
            .map_err(|source| {
                FullProgressSnapshotObservationError::CancellationCleanupValidation { source }
            })?;
        if namespace.manifest().cleanup_manifest() != cleanup_manifest {
            return Err(
                FullProgressSnapshotObservationError::CancellationCleanupNamespaceLineageMismatch,
            );
        }
        progress
            .validate_cancellation_namespace_cleanup_observation_record(
                namespace,
                cleanup_manifest,
                cleanup_recovery,
                cleanup_dispatch,
            )
            .map_err(|source| {
                FullProgressSnapshotObservationError::CancellationCleanupValidation { source }
            })?;

        let current_after_effect_generation = after_effect.recovery_generation();
        if linked_after_effect_generation == current_after_effect_generation {
            if cleanup_epoch != after_effect.execution_epoch() {
                return Err(
                    FullProgressSnapshotObservationError::CancellationCleanupAfterEffectEpochMismatch {
                        cleanup: cleanup_epoch,
                        after_effect: after_effect.execution_epoch(),
                    },
                );
            }
            if cleanup_recovery != after_effect.recovery_inventory() {
                return Err(
                    FullProgressSnapshotObservationError::CancellationCleanupAfterEffectInventoryMismatch,
                );
            }
        } else {
            if cleanup_generation >= current_after_effect_generation {
                return Err(
                    FullProgressSnapshotObservationError::CancellationCleanupGenerationNotBeforeCurrentAfterEffect {
                        cleanup: cleanup_generation,
                        after_effect: current_after_effect_generation,
                    },
                );
            }
            after_effect
                .recovery_inventory()
                .dispatch_outcome()
                .validate_transition(cleanup_dispatch, self.execution_epoch)
                .map_err(|source| {
                    FullProgressSnapshotObservationError::CancellationCleanupDispatchTransition {
                        source,
                    }
                })?;
        }
        Ok(())
    }

    fn validate_pre_publish_cancellation(
        &self,
        progress: &WorkProgress,
    ) -> Result<(), FullProgressSnapshotObservationError> {
        let Some(pre_publish) = self.pre_publish_cancellation.as_ref() else {
            if self.disposition == FullProgressDisposition::CancelledBeforePublish {
                return Err(
                    FullProgressSnapshotObservationError::CancellationPrePublishTerminalMissing,
                );
            }
            return Ok(());
        };
        let Some(dispatch) = pre_publish.dispatch_outcome() else {
            return Err(
                FullProgressSnapshotObservationError::CancellationPrePublishDispatchMissing,
            );
        };
        let inventory = self.cancellation_inventory.as_ref().ok_or_else(|| {
            FullProgressSnapshotObservationError::CancellationPrePublishValidation {
                source: WorkProgressError::CancellationInventoryMissing,
            }
        })?;
        let admission_claims = progress.admission_claims.as_ref().ok_or_else(|| {
            FullProgressSnapshotObservationError::CancellationPrePublishValidation {
                source: WorkProgressError::PrePublishCancellationAdmissionClaimsMissing,
            }
        })?;

        progress
            .validate_cancellation_dispatch_record(dispatch)
            .map_err(|source| {
                FullProgressSnapshotObservationError::CancellationPrePublishValidation { source }
            })?;
        if dispatch.recovery_generation() > self.recovery_generation {
            return Err(
                FullProgressSnapshotObservationError::CancellationPrePublishDispatchGenerationFuture {
                    dispatch: dispatch.recovery_generation(),
                    current: self.recovery_generation,
                },
            );
        }
        pre_publish.cleanup().validate().map_err(|source| {
            FullProgressSnapshotObservationError::CancellationPrePublishValidation {
                source: WorkProgressError::PrePublishCleanup(source),
            }
        })?;
        pre_publish
            .unstarted_quiescence()
            .validate()
            .map_err(|source| {
                FullProgressSnapshotObservationError::CancellationPrePublishValidation {
                    source: WorkProgressError::PrePublishUnstartedQuiescence(source),
                }
            })?;

        let eligible_keys = progress.uncheckpointed_first_effect_keys(inventory);
        let unstarted_quiescence_complete = progress
            .validate_pre_publish_unstarted_quiescence(
                inventory,
                pre_publish.unstarted_quiescence(),
                &eligible_keys,
                admission_claims,
            )
            .map_err(|source| {
                FullProgressSnapshotObservationError::CancellationPrePublishValidation { source }
            })?;
        let dispatch_complete = dispatch.rows().iter().all(|row| {
            matches!(
                row.outcome(),
                super::CancellationDispatchOutcome::NeverEntered
            )
        });
        let cleanup_complete = progress
            .validate_pre_publish_cleanup(
                inventory,
                pre_publish.cleanup(),
                admission_claims,
                unstarted_quiescence_complete && dispatch_complete,
            )
            .map_err(|source| {
                FullProgressSnapshotObservationError::CancellationPrePublishValidation { source }
            })?;
        progress
            .validate_pre_publish_settlement(
                inventory,
                admission_claims,
                pre_publish.cleanup(),
                pre_publish.settlement(),
                unstarted_quiescence_complete,
                dispatch_complete,
            )
            .map_err(|source| {
                FullProgressSnapshotObservationError::CancellationPrePublishValidation { source }
            })?;
        let settled = pre_publish
            .settlement()
            .satisfies_prepublication_cancellation(admission_claims)
            .map_err(|source| {
                FullProgressSnapshotObservationError::CancellationPrePublishValidation {
                    source: WorkProgressError::PrePublishCancellationSettlement(source),
                }
            })?;
        let terminal_ready =
            cleanup_complete && dispatch_complete && unstarted_quiescence_complete && settled;

        if terminal_ready
            && (self.disposition != FullProgressDisposition::CancelledBeforePublish
                || self.phase != super::OperationPhase::Terminal)
        {
            return Err(
                FullProgressSnapshotObservationError::CancellationPrePublishTerminalStateMismatch {
                    phase: self.phase,
                    disposition: self.disposition,
                },
            );
        }
        if self.disposition == FullProgressDisposition::CancelledBeforePublish {
            if !terminal_ready {
                return Err(
                    FullProgressSnapshotObservationError::CancellationPrePublishTerminalProofIncomplete,
                );
            }
            if self.after_effect_reconciliation.is_some()
                || self.cancellation_cleanup_observations.is_some()
            {
                return Err(
                    FullProgressSnapshotObservationError::CancellationPrePublishTerminalFollowup,
                );
            }
        }
        Ok(())
    }

    fn validate_lifecycle_consistency(
        &self,
        progress: &WorkProgress,
    ) -> Result<(), FullProgressSnapshotObservationError> {
        let supported = matches!(
            self.disposition,
            FullProgressDisposition::None
                | FullProgressDisposition::RetryPending
                | FullProgressDisposition::PartialNeedsRetry
                | FullProgressDisposition::AuditRequired
                | FullProgressDisposition::CancelRequestedBeforePublish
                | FullProgressDisposition::CancelRequestedAfterPublish
                | FullProgressDisposition::CancelRequestedAfterObservedEffect
                | FullProgressDisposition::Succeeded
                | FullProgressDisposition::SucceededWithDeferredArtifacts
                | FullProgressDisposition::CancelledBeforePublish
        );
        if !supported {
            return Err(
                FullProgressSnapshotObservationError::UnsupportedDisposition {
                    disposition: self.disposition,
                },
            );
        }

        let partial_retry_is_active = self.disposition
            == FullProgressDisposition::PartialNeedsRetry
            || (self.disposition == FullProgressDisposition::AuditRequired
                && self.active_audit.as_ref().is_some_and(|active| {
                    active.suspended_hold == super::AuditSuspendedHold::PartialNeedsRetry
                }));
        if partial_retry_is_active {
            progress
                .validate_partial_retry_eligibility()
                .map_err(|source| {
                    FullProgressSnapshotObservationError::PartialRetryEligibility { source }
                })?;
        }

        let terminal = matches!(
            self.disposition,
            FullProgressDisposition::Succeeded
                | FullProgressDisposition::SucceededWithDeferredArtifacts
                | FullProgressDisposition::CancelledBeforePublish
        );
        if terminal {
            if self.phase != OperationPhase::Terminal {
                return Err(
                    FullProgressSnapshotObservationError::TerminalPhaseMismatch {
                        disposition: self.disposition,
                        actual: self.phase,
                    },
                );
            }
        } else {
            let has_completed_or_deferred = self.steps.iter().any(|step| {
                matches!(
                    step.status,
                    ProgressStepStatus::Completed | ProgressStepStatus::Deferred
                )
            });
            let expected = if has_completed_or_deferred {
                progress.derived_phase()
            } else {
                OperationPhase::IntentDurable
            };
            if self.phase != expected {
                return Err(FullProgressSnapshotObservationError::DerivedPhaseMismatch {
                    expected,
                    actual: self.phase,
                });
            }
        }

        if self.retry_observation.is_some() {
            let retry_allowed = match self.disposition {
                FullProgressDisposition::RetryPending
                | FullProgressDisposition::PartialNeedsRetry
                | FullProgressDisposition::CancelRequestedBeforePublish
                | FullProgressDisposition::CancelRequestedAfterPublish
                | FullProgressDisposition::CancelRequestedAfterObservedEffect
                | FullProgressDisposition::CancelledBeforePublish => true,
                FullProgressDisposition::AuditRequired => {
                    self.active_audit.as_ref().is_some_and(|active| {
                        matches!(
                            active.suspended_hold,
                            super::AuditSuspendedHold::RetryPending
                                | super::AuditSuspendedHold::PartialNeedsRetry
                                | super::AuditSuspendedHold::CancelRequestedBeforePublish
                                | super::AuditSuspendedHold::CancelRequestedAfterPublish
                                | super::AuditSuspendedHold::CancelRequestedAfterObservedEffect
                        )
                    })
                }
                _ => false,
            };
            if !retry_allowed {
                return Err(
                    FullProgressSnapshotObservationError::RetryDispositionMismatch {
                        disposition: self.disposition,
                    },
                );
            }
        }

        if let Some(active) = self.active_audit.as_ref() {
            if self.phase.is_pre_publication() {
                let observed_effect_cancellation = active.suspended_hold
                    == super::AuditSuspendedHold::CancelRequestedAfterObservedEffect
                    && self
                        .cancellation_inventory
                        .as_ref()
                        .is_some_and(|inventory| {
                            inventory.route() == CancellationRoute::AfterObservedEffect
                        });
                if !observed_effect_cancellation {
                    return Err(FullProgressSnapshotObservationError::AuditPhaseInvalid {
                        phase: self.phase,
                    });
                }
            }
        }

        let claims = self.intent.admission_claims();
        let success_disposition = matches!(
            self.disposition,
            FullProgressDisposition::Succeeded
                | FullProgressDisposition::SucceededWithDeferredArtifacts
        );
        match self.claim_settlement.as_ref() {
            Some(settlement) => {
                settlement
                    .validate_normal_success_settlement(claims)
                    .map_err(FullProgressSnapshotObservationError::ClaimSettlement)?;
                let settled = settlement.all_settled_without_held();
                if !progress.all_required_complete() || !progress.optional_artifacts_resolved() {
                    return Err(
                        FullProgressSnapshotObservationError::ClaimSettlementProgressIncomplete,
                    );
                }
                if settled {
                    let expected_disposition = if progress.has_deferred_optional_artifact() {
                        FullProgressDisposition::SucceededWithDeferredArtifacts
                    } else {
                        FullProgressDisposition::Succeeded
                    };
                    if self.disposition != expected_disposition
                        || self.phase != OperationPhase::Terminal
                    {
                        return Err(
                            FullProgressSnapshotObservationError::ClaimSettlementTerminalMismatch {
                                actual_disposition: self.disposition,
                                phase: self.phase,
                            },
                        );
                    }
                } else if success_disposition {
                    return Err(
                        FullProgressSnapshotObservationError::ClaimSettlementIncompleteForSuccess,
                    );
                }
            }
            None if success_disposition => {
                return Err(
                    FullProgressSnapshotObservationError::ClaimSettlementMissingForSuccess {
                        disposition: self.disposition,
                    },
                );
            }
            None => {}
        }

        let expected_recovery_generation =
            self.after_effect_reconciliation
                .as_ref()
                .map(|after_effect| {
                    let cleanup_generation = self
                        .cancellation_cleanup_observations
                        .as_ref()
                        .map(CancellationCleanupObservation::recovery_generation)
                        .unwrap_or(0);
                    after_effect.recovery_generation().max(cleanup_generation)
                });
        match expected_recovery_generation {
            Some(expected) if self.recovery_generation != expected => Err(
                FullProgressSnapshotObservationError::RecoveryGenerationMismatch {
                    expected,
                    actual: self.recovery_generation,
                },
            ),
            None if self.pre_publish_cancellation.is_none()
                && self.cancellation_cleanup_observations.is_none()
                && self.recovery_generation != 0 =>
            {
                Err(
                    FullProgressSnapshotObservationError::RecoveryGenerationMismatch {
                        expected: 0,
                        actual: self.recovery_generation,
                    },
                )
            }
            _ => Ok(()),
        }
    }

    fn validate_retry_state(&self) -> Result<(), FullProgressSnapshotObservationError> {
        for (slot, observation) in [
            ("active", self.retry_observation.as_ref()),
            ("last", self.last_retry_observation.as_ref()),
        ] {
            let Some(observation) = observation else {
                continue;
            };
            observation.validate().map_err(|source| {
                FullProgressSnapshotObservationError::RetryObservation { slot, source }
            })?;
            if observation.lease_expiry_unix_ms() <= observation.next_retry_unix_ms() {
                return Err(FullProgressSnapshotObservationError::RetryWindowEmpty {
                    slot,
                    next_retry_unix_ms: observation.next_retry_unix_ms(),
                    lease_expiry_unix_ms: observation.lease_expiry_unix_ms(),
                });
            }
        }

        if let Some(active) = self.retry_observation.as_ref() {
            let Some(last) = self.last_retry_observation.as_ref() else {
                return Err(FullProgressSnapshotObservationError::RetryHistoryMissing);
            };
            if active != last {
                return Err(FullProgressSnapshotObservationError::RetryObservationMismatch);
            }
        }
        if self.disposition == FullProgressDisposition::RetryPending
            && self.retry_observation.is_none()
        {
            return Err(FullProgressSnapshotObservationError::RetryPendingObservationMissing);
        }
        if self.disposition == FullProgressDisposition::PartialNeedsRetry
            && self.retry_observation.is_none()
        {
            return Err(FullProgressSnapshotObservationError::PartialNeedsRetryObservationMissing);
        }
        Ok(())
    }

    fn validate_cancellation_envelope(&self) -> Result<(), FullProgressSnapshotObservationError> {
        match (
            self.cancellation_request,
            self.cancellation_inventory.as_ref(),
        ) {
            (Some(request), Some(inventory)) => {
                request
                    .validate()
                    .map_err(FullProgressSnapshotObservationError::CancellationRequest)?;
                inventory
                    .validate_against(
                        self.intent.operation_id(),
                        self.intent.plan(),
                        &self.steps,
                        self.execution_epoch,
                        self.intent.admission_claims(),
                    )
                    .map_err(FullProgressSnapshotObservationError::CancellationInventory)?;

                if self.disposition == FullProgressDisposition::AuditRequired {
                    let Some(active) = self.active_audit.as_ref() else {
                        return Err(
                            FullProgressSnapshotObservationError::CancellationAuditHoldMissing {
                                route: inventory.route(),
                            },
                        );
                    };
                    if !cancellation_audit_hold_matches_route(
                        active.suspended_hold,
                        inventory.route(),
                    ) {
                        return Err(
                            FullProgressSnapshotObservationError::CancellationAuditHoldMismatch {
                                route: inventory.route(),
                            },
                        );
                    }
                } else if !cancellation_disposition_matches_route(
                    self.disposition,
                    inventory.route(),
                ) {
                    return Err(
                        FullProgressSnapshotObservationError::CancellationDispositionMismatch {
                            route: inventory.route(),
                            disposition: self.disposition,
                        },
                    );
                }

                if self.pre_publish_cancellation.is_some()
                    && inventory.route() != CancellationRoute::BeforePublish
                {
                    return Err(
                        FullProgressSnapshotObservationError::CancellationPrePublishRouteInvalid {
                            route: inventory.route(),
                        },
                    );
                }
                if self.cancellation_cleanup_observations.is_some()
                    && self.after_effect_reconciliation.is_none()
                {
                    return Err(
                        FullProgressSnapshotObservationError::CancellationCleanupAfterEffectMissing,
                    );
                }
            }
            (Some(_), None) | (None, Some(_)) => {
                return Err(
                    FullProgressSnapshotObservationError::CancellationEnvelopePairMismatch {
                        request_present: self.cancellation_request.is_some(),
                        inventory_present: self.cancellation_inventory.is_some(),
                    },
                );
            }
            (None, None) => {
                if cancellation_disposition_requires_envelope(self.disposition) {
                    return Err(
                        FullProgressSnapshotObservationError::CancellationDispositionMissingEnvelope {
                            disposition: self.disposition,
                        },
                    );
                }
                if self
                    .active_audit
                    .as_ref()
                    .is_some_and(|active| cancellation_audit_hold(active.suspended_hold))
                {
                    return Err(
                        FullProgressSnapshotObservationError::CancellationAuditHoldOrphaned,
                    );
                }
                if self.pre_publish_cancellation.is_some() {
                    return Err(
                        FullProgressSnapshotObservationError::CancellationObservationOrphaned {
                            field: "pre_publish_cancellation",
                        },
                    );
                }
                if self.after_effect_reconciliation.is_some() {
                    return Err(
                        FullProgressSnapshotObservationError::CancellationObservationOrphaned {
                            field: "after_effect_reconciliation",
                        },
                    );
                }
                if self.cancellation_cleanup_observations.is_some() {
                    return Err(
                        FullProgressSnapshotObservationError::CancellationObservationOrphaned {
                            field: "cancellation_cleanup_observations",
                        },
                    );
                }
            }
        }
        Ok(())
    }

    fn validate_audit_state(&self) -> Result<(), FullProgressSnapshotObservationError> {
        match self.active_audit.as_ref() {
            Some(active) => {
                if self.disposition != FullProgressDisposition::AuditRequired {
                    return Err(
                        FullProgressSnapshotObservationError::AuditDispositionMismatch {
                            disposition: self.disposition,
                        },
                    );
                }
                if active.generation == 0 {
                    return Err(FullProgressSnapshotObservationError::AuditGenerationZero);
                }
                if active.generation != self.audit_generation {
                    return Err(
                        FullProgressSnapshotObservationError::AuditGenerationMismatch {
                            active: active.generation,
                            retained: self.audit_generation,
                        },
                    );
                }
                active
                    .observation
                    .validate_for_plan(self.intent.plan())
                    .map_err(FullProgressSnapshotObservationError::AuditObservation)?;
                match active.suspended_hold {
                    super::AuditSuspendedHold::RetryPending
                    | super::AuditSuspendedHold::PartialNeedsRetry
                        if self.retry_observation.is_none() =>
                    {
                        return Err(FullProgressSnapshotObservationError::AuditRetryHoldMissing);
                    }
                    super::AuditSuspendedHold::CancelRequestedBeforePublish
                    | super::AuditSuspendedHold::CancelRequestedAfterPublish
                    | super::AuditSuspendedHold::CancelRequestedAfterObservedEffect
                        if self.cancellation_request.is_none() =>
                    {
                        return Err(
                            FullProgressSnapshotObservationError::AuditCancellationHoldMissing,
                        );
                    }
                    _ => {}
                }
            }
            None if self.disposition == FullProgressDisposition::AuditRequired => {
                return Err(FullProgressSnapshotObservationError::AuditStateMissing);
            }
            None => {}
        }
        Ok(())
    }

    fn validate_against_progress(
        &self,
        progress: &WorkProgress,
    ) -> Result<(), FullProgressSnapshotObservationError> {
        if self.intent.operation_id() != progress.operation_id {
            return Err(FullProgressSnapshotObservationError::OperationIdMismatch);
        }
        if self.intent.plan() != &progress.plan {
            return Err(FullProgressSnapshotObservationError::PlanMismatch);
        }
        let Some(progress_claims) = progress.admission_claims.as_ref() else {
            return Err(FullProgressSnapshotObservationError::AdmissionClaimsMissing);
        };
        if progress_claims != self.intent.admission_claims() {
            return Err(FullProgressSnapshotObservationError::AdmissionClaimsMismatch);
        }
        if progress.origin_policies
            != super::derive_origin_policies(self.intent.args(), self.intent.plan())
        {
            return Err(FullProgressSnapshotObservationError::OriginPoliciesMismatch);
        }
        if self.scopes != progress.scopes {
            return Err(FullProgressSnapshotObservationError::CurrentScopesMismatch);
        }
        if self.steps != progress.steps {
            return Err(FullProgressSnapshotObservationError::CurrentStepsMismatch);
        }
        self.validate_mutable_state(progress)
    }

    fn validation_progress(&self) -> WorkProgress {
        WorkProgress {
            operation_id: self.intent.operation_id(),
            plan: self.intent.plan().clone(),
            scopes: self.scopes.clone(),
            steps: self.steps.clone(),
            phase: self.phase,
            disposition: self.disposition,
            retry_observation: self.retry_observation.clone(),
            last_retry_observation: self.last_retry_observation.clone(),
            execution_epoch: self.execution_epoch,
            active_audit: self.active_audit.clone(),
            audit_generation: self.audit_generation,
            cancellation_request: self.cancellation_request,
            admission_claims: Some(self.intent.admission_claims().clone()),
            claim_settlement: self.claim_settlement.clone(),
            cancellation_inventory: self.cancellation_inventory.clone(),
            origin_policies: super::derive_origin_policies(self.intent.args(), self.intent.plan()),
            recovery_generation: self.recovery_generation,
            pre_publish_cancellation: self.pre_publish_cancellation.clone(),
            after_effect_reconciliation: self.after_effect_reconciliation.clone(),
            cancellation_cleanup_observations: self.cancellation_cleanup_observations.clone(),
            readiness_deferral_batches: self.readiness_deferral_batches.clone(),
        }
    }

    fn validate_scopes(&self) -> Result<(), FullProgressSnapshotObservationError> {
        let descriptors = self.intent.participants().descriptors();
        if self.scopes.len() != descriptors.len() {
            return Err(FullProgressSnapshotObservationError::ScopeCountMismatch {
                actual: self.scopes.len(),
                expected: descriptors.len(),
            });
        }
        for (index, (scope, descriptor)) in self.scopes.iter().zip(descriptors.iter()).enumerate() {
            if scope.participant_id() != descriptor.id() || scope.kind() != descriptor.kind() {
                return Err(FullProgressSnapshotObservationError::ScopeIdentityMismatch { index });
            }
            if let Some(fence) = scope.source_fence() {
                fence.validate().map_err(|source| {
                    FullProgressSnapshotObservationError::ScopeFence { index, source }
                })?;
            }
            let initial_scope = ParticipantScope::from_descriptor(descriptor);
            if !historical_scope_matches_current(self.execution_epoch, 0, scope, &initial_scope) {
                return Err(
                    FullProgressSnapshotObservationError::ScopeHistoricalMismatch { index },
                );
            }
        }
        for (index, descriptor) in descriptors.iter().enumerate() {
            let Some(initial_fence) = descriptor.source_fence() else {
                continue;
            };
            let group_indices = descriptors
                .iter()
                .enumerate()
                .filter_map(|(group_index, group_descriptor)| {
                    group_descriptor
                        .source_fence()
                        .filter(|group_fence| {
                            same_stable_source_identity(initial_fence, group_fence)
                        })
                        .map(|_| group_index)
                })
                .collect::<Vec<_>>();
            let changed = group_indices.iter().any(|&group_index| {
                self.scopes[group_index].source_fence() != descriptors[group_index].source_fence()
            });
            if !changed {
                continue;
            }
            let current_fence = self.scopes[index]
                .source_fence()
                .expect("historical scope validation retained source fence");
            for &matching_index in &group_indices {
                let matching_fence = self.scopes[matching_index]
                    .source_fence()
                    .expect("historical scope validation retained source fence");
                if matching_fence != current_fence {
                    return Err(
                        FullProgressSnapshotObservationError::ScopeStableFenceMismatch {
                            index,
                            matching_index,
                        },
                    );
                }
            }
            let maximum_initial_lease = group_indices
                .iter()
                .map(|&group_index| {
                    descriptors[group_index]
                        .source_fence()
                        .expect("source-fenced stable identity group")
                        .lease_epoch()
                })
                .max()
                .expect("nonempty source identity group");
            if current_fence.lease_epoch() <= maximum_initial_lease {
                return Err(
                    FullProgressSnapshotObservationError::ScopeStableFenceLeaseNotAdvanced {
                        index,
                        current_lease_epoch: current_fence.lease_epoch(),
                        maximum_initial_lease,
                    },
                );
            }
        }
        Ok(())
    }

    fn validate_step_bounds(&self) -> Result<(), FullProgressSnapshotObservationError> {
        if self.steps.len() > super::super::work_plan::MAX_WORK_PLAN_UNITS {
            return Err(FullProgressSnapshotObservationError::TooManySteps {
                actual: self.steps.len(),
                maximum: super::super::work_plan::MAX_WORK_PLAN_UNITS,
            });
        }
        let expected = self.intent.plan().units().len();
        if self.steps.len() != expected {
            return Err(FullProgressSnapshotObservationError::StepCountMismatch {
                actual: self.steps.len(),
                expected,
            });
        }
        Ok(())
    }

    fn validate_steps(
        &self,
        progress: &WorkProgress,
    ) -> Result<(), FullProgressSnapshotObservationError> {
        let units = progress.plan.units();
        for (index, (step, unit)) in self.steps.iter().zip(units.iter()).enumerate() {
            if step.key != unit.key() {
                return Err(FullProgressSnapshotObservationError::StepKeyMismatch {
                    index,
                    actual: step.key,
                    expected: unit.key(),
                });
            }
            if step.requirement != unit.requirement() {
                return Err(
                    FullProgressSnapshotObservationError::StepRequirementMismatch { key: step.key },
                );
            }
            match (unit.applicability(), step.status) {
                (WorkUnitApplicability::NotApplicable, ProgressStepStatus::NotApplicable)
                | (WorkUnitApplicability::Required, ProgressStepStatus::Pending)
                | (WorkUnitApplicability::Required, ProgressStepStatus::Completed)
                | (WorkUnitApplicability::Required, ProgressStepStatus::Deferred) => {}
                _ => {
                    return Err(
                        FullProgressSnapshotObservationError::StepApplicabilityMismatch {
                            key: step.key,
                            applicability: unit.applicability(),
                            status: step.status,
                        },
                    );
                }
            }
            if step.status == ProgressStepStatus::Deferred
                && (step.requirement != ParticipantRequirement::OptionalRebuildable
                    || step.key.role() != WorkUnitRole::Artifact)
            {
                return Err(FullProgressSnapshotObservationError::DeferredRoleInvalid {
                    key: step.key,
                });
            }
            match (
                step.status,
                step.evidence.as_ref(),
                step.completion_context.as_ref(),
            ) {
                (ProgressStepStatus::Completed, Some(evidence), Some(context)) => {
                    progress
                        .validate_normal_historical_completion(step.key, context, evidence)
                        .map_err(|source| FullProgressSnapshotObservationError::Completion {
                            source,
                        })?;
                    for edge in progress
                        .plan
                        .prerequisites()
                        .iter()
                        .filter(|edge| edge.dependent() == step.key)
                    {
                        let prerequisite = self
                            .steps
                            .iter()
                            .find(|candidate| candidate.key == edge.prerequisite())
                            .ok_or(FullProgressSnapshotObservationError::PrerequisiteMissing {
                                dependent: step.key,
                                prerequisite: edge.prerequisite(),
                            })?;
                        if !matches!(
                            prerequisite.status,
                            ProgressStepStatus::Completed | ProgressStepStatus::NotApplicable
                        ) {
                            return Err(
                                FullProgressSnapshotObservationError::PrerequisiteIncomplete {
                                    dependent: step.key,
                                    prerequisite: prerequisite.key,
                                },
                            );
                        }
                        if let Some(prerequisite_context) = prerequisite.completion_context.as_ref()
                        {
                            if prerequisite_context.execution_epoch() > context.execution_epoch() {
                                return Err(
                                    FullProgressSnapshotObservationError::PrerequisiteEpochInversion {
                                        dependent: step.key,
                                        prerequisite: prerequisite.key,
                                        prerequisite_epoch: prerequisite_context.execution_epoch(),
                                        dependent_epoch: context.execution_epoch(),
                                    },
                                );
                            }
                        }
                    }
                }
                (ProgressStepStatus::Completed, None, _) => {
                    return Err(FullProgressSnapshotObservationError::Completion {
                        source: WorkProgressError::CompletionEvidenceMissing { key: step.key },
                    });
                }
                (ProgressStepStatus::Completed, Some(_), None) => {
                    return Err(FullProgressSnapshotObservationError::Completion {
                        source: WorkProgressError::CompletionContextMissing { key: step.key },
                    });
                }
                (_, Some(_), _) => {
                    return Err(FullProgressSnapshotObservationError::Completion {
                        source: WorkProgressError::CompletionEvidenceUnexpected { key: step.key },
                    });
                }
                (_, _, Some(_)) => {
                    return Err(FullProgressSnapshotObservationError::Completion {
                        source: WorkProgressError::CompletionContextUnexpected { key: step.key },
                    });
                }
                (_, None, None) => {}
            }
        }
        Ok(())
    }

    fn validate_mutable_state(
        &self,
        progress: &WorkProgress,
    ) -> Result<(), FullProgressSnapshotObservationError> {
        let mismatch = |field| FullProgressSnapshotObservationError::MutableStateMismatch { field };
        if self.phase != progress.phase {
            return Err(mismatch("phase"));
        }
        if self.disposition != progress.disposition {
            return Err(mismatch("disposition"));
        }
        if self.retry_observation != progress.retry_observation {
            return Err(mismatch("retry_observation"));
        }
        if self.last_retry_observation != progress.last_retry_observation {
            return Err(mismatch("last_retry_observation"));
        }
        if self.execution_epoch != progress.execution_epoch {
            return Err(mismatch("execution_epoch"));
        }
        if self.active_audit != progress.active_audit {
            return Err(mismatch("active_audit"));
        }
        if self.audit_generation != progress.audit_generation {
            return Err(mismatch("audit_generation"));
        }
        if self.cancellation_request != progress.cancellation_request {
            return Err(mismatch("cancellation_request"));
        }
        if self.claim_settlement != progress.claim_settlement {
            return Err(mismatch("claim_settlement"));
        }
        if self.cancellation_inventory != progress.cancellation_inventory {
            return Err(mismatch("cancellation_inventory"));
        }
        if self.recovery_generation != progress.recovery_generation {
            return Err(mismatch("recovery_generation"));
        }
        if self.pre_publish_cancellation != progress.pre_publish_cancellation {
            return Err(mismatch("pre_publish_cancellation"));
        }
        if self.after_effect_reconciliation != progress.after_effect_reconciliation {
            return Err(mismatch("after_effect_reconciliation"));
        }
        if self.cancellation_cleanup_observations != progress.cancellation_cleanup_observations {
            return Err(mismatch("cancellation_cleanup_observations"));
        }
        if self.readiness_deferral_batches != progress.readiness_deferral_batches {
            return Err(mismatch("readiness_deferral_batches"));
        }
        Ok(())
    }
}

fn cancellation_disposition_requires_envelope(disposition: FullProgressDisposition) -> bool {
    matches!(
        disposition,
        FullProgressDisposition::CancelRequestedBeforePublish
            | FullProgressDisposition::CancelRequestedAfterPublish
            | FullProgressDisposition::CancelRequestedAfterObservedEffect
            | FullProgressDisposition::CancelledBeforePublish
            | FullProgressDisposition::CancelledAfterPublish
            | FullProgressDisposition::CancelledAfterObservedEffect
    )
}

fn cancellation_disposition_matches_route(
    disposition: FullProgressDisposition,
    route: CancellationRoute,
) -> bool {
    match route {
        CancellationRoute::BeforePublish => matches!(
            disposition,
            FullProgressDisposition::CancelRequestedBeforePublish
                | FullProgressDisposition::CancelledBeforePublish
        ),
        CancellationRoute::AfterPublish => matches!(
            disposition,
            FullProgressDisposition::CancelRequestedAfterPublish
                | FullProgressDisposition::CancelledAfterPublish
        ),
        CancellationRoute::AfterObservedEffect => matches!(
            disposition,
            FullProgressDisposition::CancelRequestedAfterObservedEffect
                | FullProgressDisposition::CancelledAfterObservedEffect
        ),
    }
}

fn cancellation_audit_hold(hold: super::AuditSuspendedHold) -> bool {
    matches!(
        hold,
        super::AuditSuspendedHold::CancelRequestedBeforePublish
            | super::AuditSuspendedHold::CancelRequestedAfterPublish
            | super::AuditSuspendedHold::CancelRequestedAfterObservedEffect
    )
}

fn cancellation_audit_hold_matches_route(
    hold: super::AuditSuspendedHold,
    route: CancellationRoute,
) -> bool {
    match route {
        CancellationRoute::BeforePublish => {
            hold == super::AuditSuspendedHold::CancelRequestedBeforePublish
        }
        CancellationRoute::AfterPublish => {
            hold == super::AuditSuspendedHold::CancelRequestedAfterPublish
        }
        CancellationRoute::AfterObservedEffect => {
            hold == super::AuditSuspendedHold::CancelRequestedAfterObservedEffect
        }
    }
}

/// Failure while validating the partial full-progress snapshot observation boundary.
#[allow(missing_docs)]
#[derive(Clone, Debug, Error, PartialEq)]
pub enum FullProgressSnapshotObservationError {
    /// Retained compact readiness deferral proof failed WorkProgress validation.
    #[error("invalid full-progress snapshot readiness deferral history: {source}")]
    ReadinessDeferralValidation {
        #[source]
        source: WorkProgressError,
    },
    /// The retained immutable intent failed its own consistency checks.
    #[error("invalid full-progress snapshot intent: {0}")]
    Intent(#[source] FullProgressIntentObservationError),
    /// The snapshot intent operation identity differs from progress.
    #[error("full-progress snapshot operation identity does not match progress")]
    OperationIdMismatch,
    /// The snapshot intent plan differs from progress.
    #[error("full-progress snapshot plan does not match progress")]
    PlanMismatch,
    /// Progress did not retain the admission claims required to bind this snapshot.
    #[error("full-progress snapshot requires retained admission claims")]
    AdmissionClaimsMissing,
    /// The snapshot intent claims differ from retained progress claims.
    #[error("full-progress snapshot admission claims do not match progress")]
    AdmissionClaimsMismatch,
    /// Origin policies derived from the frozen intent differ from progress.
    #[error("full-progress snapshot origin policies do not match progress")]
    OriginPoliciesMismatch,
    /// The current scope collection had the wrong size.
    #[error("full-progress snapshot has {actual} scopes, expected {expected}")]
    ScopeCountMismatch { actual: usize, expected: usize },
    /// The candidate scopes differ from current progress scopes.
    #[error("full-progress snapshot scopes do not match current progress")]
    CurrentScopesMismatch,
    /// A scope ID or kind differs from its initial participant descriptor.
    #[error("full-progress snapshot scope identity differs at index {index}")]
    ScopeIdentityMismatch { index: usize },
    /// A current source fence failed its bounded shape validation.
    #[error("invalid full-progress snapshot scope fence at index {index}: {source}")]
    ScopeFence {
        index: usize,
        #[source]
        source: ParticipantValidationError,
    },
    /// A current scope cannot be reconciled with its initial descriptor under historical rules.
    #[error(
        "full-progress snapshot scope at index {index} is not compatible with its initial fence"
    )]
    ScopeHistoricalMismatch { index: usize },
    /// Current scopes sharing one stable source identity carried different fences.
    #[error(
        "full-progress snapshot scopes at indexes {index} and {matching_index} disagree on their current stable-source fence"
    )]
    ScopeStableFenceMismatch { index: usize, matching_index: usize },
    /// A changed stable-source group did not move to a strictly newer common lease.
    #[error(
        "full-progress snapshot scope at index {index} has current lease {current_lease_epoch}, but stable-source group maximum initial lease is {maximum_initial_lease}"
    )]
    ScopeStableFenceLeaseNotAdvanced {
        index: usize,
        current_lease_epoch: u64,
        maximum_initial_lease: u64,
    },
    /// The snapshot exceeded the frozen plan bound.
    #[error("full-progress snapshot has {actual} steps, maximum is {maximum}")]
    TooManySteps { actual: usize, maximum: usize },
    /// The retained deferral batch count exceeded the frozen-plan bound.
    #[error("full-progress snapshot has {actual} readiness deferral batches, maximum is {maximum}")]
    TooManyReadinessDeferralBatches { actual: usize, maximum: usize },
    /// The aggregate retained deferral keys or rows exceeded the frozen-plan bound.
    #[error(
        "full-progress snapshot has {keys} readiness deferral keys and {rows} rows, maximum is {maximum}"
    )]
    TooManyReadinessDeferralObservations {
        /// Aggregate selected artifact keys.
        keys: usize,
        /// Aggregate readiness rows.
        rows: usize,
        /// Maximum permitted aggregate observations.
        maximum: usize,
    },
    /// The snapshot had the wrong number of progress rows.
    #[error("full-progress snapshot has {actual} steps, expected {expected}")]
    StepCountMismatch { actual: usize, expected: usize },
    /// A retained retry observation failed its bounded validation.
    #[error("invalid full-progress snapshot {slot} retry observation: {source}")]
    RetryObservation {
        /// Retry observation slot being validated.
        slot: &'static str,
        #[source]
        /// Underlying retry observation error.
        source: RetryObservationValidationError,
    },
    /// A retained retry lease ended at or before its next retry boundary.
    #[error(
        "full-progress snapshot {slot} retry lease expires at {lease_expiry_unix_ms}, not after next retry {next_retry_unix_ms}"
    )]
    RetryWindowEmpty {
        /// Retry observation slot being validated.
        slot: &'static str,
        /// Next retry timestamp.
        next_retry_unix_ms: i64,
        /// Lease expiry timestamp.
        lease_expiry_unix_ms: i64,
    },
    /// An active retry did not retain the exact historical retry observation.
    #[error("full-progress snapshot active retry observation has no matching last observation")]
    RetryHistoryMissing,
    /// Active and last retry observations disagree.
    #[error("full-progress snapshot active and last retry observations differ")]
    RetryObservationMismatch,
    /// RetryPending disposition had no active retry observation.
    #[error("full-progress snapshot RetryPending disposition has no active retry observation")]
    RetryPendingObservationMissing,
    /// PartialNeedsRetry disposition had no active retry observation.
    #[error("full-progress snapshot PartialNeedsRetry disposition has no active retry observation")]
    PartialNeedsRetryObservationMissing,
    /// PartialNeedsRetry did not retain the required completed and pending work shape.
    #[error("full-progress snapshot partial retry eligibility is invalid: {source}")]
    PartialRetryEligibility {
        #[source]
        source: WorkProgressError,
    },
    /// The snapshot retained a disposition that the current full-progress state machine does not
    /// yet produce or validate.
    #[error(
        "full-progress snapshot disposition {disposition:?} is unsupported in the current state model"
    )]
    UnsupportedDisposition {
        disposition: FullProgressDisposition,
    },
    /// A terminal disposition was retained before the terminal phase.
    #[error(
        "full-progress snapshot terminal disposition {disposition:?} has phase {actual:?}, expected Terminal"
    )]
    TerminalPhaseMismatch {
        disposition: FullProgressDisposition,
        actual: OperationPhase,
    },
    /// A nonterminal snapshot phase did not match the exact phase derived from its retained rows.
    #[error("full-progress snapshot phase {actual:?} does not match derived phase {expected:?}")]
    DerivedPhaseMismatch {
        expected: OperationPhase,
        actual: OperationPhase,
    },
    /// An active retry observation was retained under a disposition that cannot suspend it.
    #[error(
        "full-progress snapshot active retry observation is incompatible with disposition {disposition:?}"
    )]
    RetryDispositionMismatch {
        disposition: FullProgressDisposition,
    },
    /// An audit overlay was retained before publication without the one supported observed-effect
    /// cancellation hold.
    #[error("full-progress snapshot audit overlay is invalid in prepublication phase {phase:?}")]
    AuditPhaseInvalid { phase: OperationPhase },
    /// The retained normal claim settlement failed exact coverage or retained-charge validation.
    #[error("invalid full-progress snapshot normal claim settlement: {0}")]
    ClaimSettlement(#[source] AdmissionClaimValidationError),
    /// A normal success disposition did not retain a claim settlement observation.
    #[error("full-progress snapshot success disposition {disposition:?} has no claim settlement")]
    ClaimSettlementMissingForSuccess {
        disposition: FullProgressDisposition,
    },
    /// A normal success disposition retained a held claim settlement.
    #[error("full-progress snapshot success disposition has an unsettled claim")]
    ClaimSettlementIncompleteForSuccess,
    /// All retained claims were settled while required work or optional-artifact decisions remain.
    #[error("full-progress snapshot claim settlement is ahead of retained work progress")]
    ClaimSettlementProgressIncomplete,
    /// A fully settled normal claim inventory did not match the exact normal terminal disposition.
    #[error(
        "full-progress snapshot settled claims require a normal success disposition and Terminal phase, found {actual_disposition:?} and {phase:?}"
    )]
    ClaimSettlementTerminalMismatch {
        actual_disposition: FullProgressDisposition,
        phase: OperationPhase,
    },
    /// A retained audit observation failed its bounded target validation.
    #[error("invalid full-progress snapshot audit observation: {0}")]
    AuditObservation(#[source] AuditTargetValidationError),
    /// AuditRequired disposition had no active audit overlay.
    #[error("full-progress snapshot AuditRequired disposition has no active audit overlay")]
    AuditStateMissing,
    /// An active audit overlay was retained under another disposition.
    #[error("full-progress snapshot active audit is retained under disposition {disposition:?}")]
    AuditDispositionMismatch {
        disposition: FullProgressDisposition,
    },
    /// An active audit overlay must have a positive generation.
    #[error("full-progress snapshot active audit generation must be positive")]
    AuditGenerationZero,
    /// Active audit generation differed from retained audit generation.
    #[error(
        "full-progress snapshot active audit generation {active} differs from retained generation {retained}"
    )]
    AuditGenerationMismatch { active: u64, retained: u64 },
    /// An audit suspended a retry without retaining its retry observation.
    #[error("full-progress snapshot audit retry hold has no retry observation")]
    AuditRetryHoldMissing,
    /// An audit suspended cancellation without retaining its request observation.
    #[error("full-progress snapshot audit cancellation hold has no cancellation request")]
    AuditCancellationHoldMissing,
    /// A cancellation request and its frozen inventory were not retained as a pair.
    #[error(
        "full-progress snapshot cancellation envelope is incomplete (request present: {request_present}, inventory present: {inventory_present})"
    )]
    CancellationEnvelopePairMismatch {
        request_present: bool,
        inventory_present: bool,
    },
    /// A cancellation request observation failed its bounded validation.
    #[error("invalid full-progress snapshot cancellation request: {0}")]
    CancellationRequest(#[source] CancellationRequestValidationError),
    /// A frozen cancellation inventory failed validation against the snapshot intent and rows.
    #[error("invalid full-progress snapshot cancellation inventory: {0}")]
    CancellationInventory(#[source] CancellationInventoryError),
    /// A cancellation disposition did not match the frozen inventory route.
    #[error(
        "full-progress snapshot cancellation route {route:?} does not match disposition {disposition:?}"
    )]
    CancellationDispositionMismatch {
        route: CancellationRoute,
        disposition: FullProgressDisposition,
    },
    /// A cancellation disposition was retained without its request/inventory envelope.
    #[error("full-progress snapshot disposition {disposition:?} requires a cancellation envelope")]
    CancellationDispositionMissingEnvelope {
        disposition: FullProgressDisposition,
    },
    /// AuditRequired retained a cancellation route without an active cancellation hold.
    #[error("full-progress snapshot cancellation route {route:?} has no matching audit hold")]
    CancellationAuditHoldMissing { route: CancellationRoute },
    /// AuditRequired retained a hold for another cancellation route.
    #[error("full-progress snapshot audit cancellation hold does not match route {route:?}")]
    CancellationAuditHoldMismatch { route: CancellationRoute },
    /// An audit cancellation hold was retained without a cancellation envelope.
    #[error("full-progress snapshot audit cancellation hold has no cancellation envelope")]
    CancellationAuditHoldOrphaned,
    /// A retained prepublication observation was paired with a non-before-publish route.
    #[error(
        "full-progress snapshot prepublication cancellation observation is invalid for route {route:?}"
    )]
    CancellationPrePublishRouteInvalid { route: CancellationRoute },
    /// A retained terminal before-publication cancellation had no stored prepublication record.
    #[error(
        "full-progress snapshot terminal before-publication cancellation has no prepublication observation"
    )]
    CancellationPrePublishTerminalMissing,
    /// A retained prepublication cancellation was decoded from a legacy value without dispatch.
    #[error("full-progress snapshot prepublication cancellation has no dispatch outcome")]
    CancellationPrePublishDispatchMissing,
    /// A nested prepublication cancellation validator rejected the retained observations.
    #[error("invalid full-progress snapshot prepublication cancellation: {source}")]
    CancellationPrePublishValidation {
        #[source]
        source: WorkProgressError,
    },
    /// The retained prepublication dispatch generation exceeds the snapshot generation.
    #[error(
        "full-progress snapshot prepublication dispatch generation {dispatch} is newer than recovery generation {current}"
    )]
    CancellationPrePublishDispatchGenerationFuture { dispatch: u64, current: u64 },
    /// Complete prepublication evidence was retained under a nonterminal phase/disposition.
    #[error(
        "full-progress snapshot prepublication cancellation is complete but phase {phase:?} and disposition {disposition:?} are not terminal before-publish"
    )]
    CancellationPrePublishTerminalStateMismatch {
        phase: OperationPhase,
        disposition: FullProgressDisposition,
    },
    /// A terminal before-publication cancellation retained incomplete proof.
    #[error("full-progress snapshot terminal before-publication cancellation lacks complete proof")]
    CancellationPrePublishTerminalProofIncomplete,
    /// Terminal before-publication cancellation advanced into later observations.
    #[error(
        "full-progress snapshot terminal before-publication cancellation retains after-effect or cleanup observations"
    )]
    CancellationPrePublishTerminalFollowup,
    /// The global recovery counter did not match the accepted after-effect/cleanup generations.
    #[error(
        "full-progress snapshot recovery generation {actual} does not match expected generation {expected}"
    )]
    RecoveryGenerationMismatch { expected: u64, actual: u64 },
    /// Cleanup observations require a retained after-effect observation.
    #[error(
        "full-progress snapshot cleanup observations have no retained after-effect observation"
    )]
    CancellationCleanupAfterEffectMissing,
    /// A cancellation-derived observation was retained without its envelope.
    #[error("full-progress snapshot cancellation observation {field} has no cancellation envelope")]
    CancellationObservationOrphaned { field: &'static str },
    /// Retained cleanup observations belong to a future snapshot epoch.
    #[error(
        "full-progress snapshot cleanup observation epoch {cleanup} is newer than current epoch {current}"
    )]
    CancellationCleanupExecutionEpochFuture { cleanup: u64, current: u64 },
    /// The resource-cleanup record was not captured at the cleanup observation epoch.
    #[error(
        "full-progress snapshot resource-cleanup observation epoch {actual} does not match cleanup epoch {cleanup}"
    )]
    CancellationCleanupResourceEpochMismatch { cleanup: u64, actual: u64 },
    /// The namespace-cleanup record was not captured at the cleanup observation epoch.
    #[error(
        "full-progress snapshot namespace-cleanup observation epoch {actual} does not match cleanup epoch {cleanup}"
    )]
    CancellationCleanupNamespaceEpochMismatch { cleanup: u64, actual: u64 },
    /// The cleanup lineage dispatch record was not captured at the cleanup observation epoch.
    #[error(
        "full-progress snapshot cleanup dispatch epoch {actual} does not match cleanup epoch {cleanup}"
    )]
    CancellationCleanupDispatchEpochMismatch { cleanup: u64, actual: u64 },
    /// The cleanup record did not link to a positive after-effect generation.
    #[error("full-progress snapshot cleanup after-effect generation must be positive")]
    CancellationCleanupAfterEffectGenerationZero,
    /// The cleanup observation did not retain a positive global recovery generation.
    #[error("full-progress snapshot cleanup recovery generation must be positive")]
    CancellationCleanupGenerationZero,
    /// The cleanup global generation did not follow its linked after-effect generation.
    #[error(
        "full-progress snapshot cleanup after-effect generation {after_effect} is not before cleanup generation {cleanup}"
    )]
    CancellationCleanupAfterEffectGenerationNotBeforeCleanup { after_effect: u64, cleanup: u64 },
    /// The cleanup global generation is newer than the snapshot generation.
    #[error(
        "full-progress snapshot cleanup generation {cleanup} is newer than current recovery generation {current}"
    )]
    CancellationCleanupGenerationFuture { cleanup: u64, current: u64 },
    /// The retained cleanup dispatch generation is newer than its linked after-effect generation.
    #[error(
        "full-progress snapshot cleanup dispatch generation {dispatch} is newer than linked after-effect generation {after_effect}"
    )]
    CancellationCleanupDispatchGenerationFuture { dispatch: u64, after_effect: u64 },
    /// The cleanup record links to an after-effect generation newer than the retained state.
    #[error(
        "full-progress snapshot cleanup after-effect generation {cleanup} is newer than retained generation {current}"
    )]
    CancellationCleanupAfterEffectGenerationFuture { cleanup: u64, current: u64 },
    /// A cleanup record's resource and namespace manifests retained different lineage.
    #[error("full-progress snapshot cleanup resource and namespace lineages do not match")]
    CancellationCleanupNamespaceLineageMismatch,
    /// A nested cleanup validator rejected the retained historical observations.
    #[error("invalid full-progress snapshot cancellation cleanup observation: {source}")]
    CancellationCleanupValidation {
        #[source]
        source: WorkProgressError,
    },
    /// A retained cleanup dispatch did not monotonically refine the current after-effect dispatch.
    #[error("invalid full-progress snapshot cleanup dispatch transition: {source}")]
    CancellationCleanupDispatchTransition {
        #[source]
        source: CancellationDispatchOutcomeRecordError,
    },
    /// Cleanup from an older after-effect generation was retained after a newer generation without
    /// leaving room for the cleanup generation between them.
    #[error(
        "full-progress snapshot cleanup generation {cleanup} is not before current after-effect generation {after_effect}"
    )]
    CancellationCleanupGenerationNotBeforeCurrentAfterEffect { cleanup: u64, after_effect: u64 },
    /// Cleanup linked to the current after-effect generation was captured at another epoch.
    #[error(
        "full-progress snapshot cleanup epoch {cleanup} does not match linked after-effect epoch {after_effect}"
    )]
    CancellationCleanupAfterEffectEpochMismatch { cleanup: u64, after_effect: u64 },
    /// Cleanup linked to the current after-effect generation retained another recovery inventory.
    #[error("full-progress snapshot cleanup recovery inventory does not match linked after-effect")]
    CancellationCleanupAfterEffectInventoryMismatch,
    /// The accepted after-effect observation belongs to a future snapshot epoch.
    #[error(
        "full-progress snapshot after-effect execution epoch {accepted} is newer than current epoch {current}"
    )]
    AfterEffectExecutionEpochFuture { accepted: u64, current: u64 },
    /// The retained dispatch record was captured at another accepted after-effect epoch.
    #[error(
        "full-progress snapshot after-effect dispatch epoch {actual} does not match accepted epoch {accepted}"
    )]
    AfterEffectDispatchEpochMismatch { accepted: u64, actual: u64 },
    /// The retained source-closure record was captured at another accepted after-effect epoch.
    #[error(
        "full-progress snapshot after-effect source-closure epoch {actual} does not match accepted epoch {accepted}"
    )]
    AfterEffectSourceClosureEpochMismatch { accepted: u64, actual: u64 },
    /// The retained coordinator-closure record was captured at another accepted after-effect epoch.
    #[error(
        "full-progress snapshot after-effect coordinator-closure epoch {actual} does not match accepted epoch {accepted}"
    )]
    AfterEffectCoordinatorClosureEpochMismatch { accepted: u64, actual: u64 },
    /// The accepted after-effect generation was zero.
    #[error("full-progress snapshot after-effect recovery generation must be positive")]
    AfterEffectGenerationZero,
    /// The accepted after-effect generation is newer than the snapshot's global generation.
    #[error(
        "full-progress snapshot after-effect generation {accepted} is newer than current recovery generation {current}"
    )]
    AfterEffectGenerationFuture { accepted: u64, current: u64 },
    /// The retained dispatch record had no positive recovery generation.
    #[error("full-progress snapshot after-effect dispatch recovery generation must be positive")]
    AfterEffectDispatchGenerationZero,
    /// The retained dispatch generation is newer than the accepted after-effect generation.
    #[error(
        "full-progress snapshot after-effect dispatch generation {actual} is newer than accepted generation {accepted}"
    )]
    AfterEffectDispatchGenerationFuture { accepted: u64, actual: u64 },
    /// A delegated retained after-effect validator rejected the nested observations.
    #[error("invalid full-progress snapshot after-effect reconciliation: {source}")]
    AfterEffectValidation {
        #[source]
        source: WorkProgressError,
    },
    /// The retained after-effect dispatch did not monotonically refine prepublication dispatch.
    #[error("invalid full-progress snapshot after-effect dispatch transition: {source}")]
    AfterEffectDispatchTransition {
        #[source]
        source: CancellationDispatchOutcomeRecordError,
    },
    /// The accepted after-effect generation did not follow retained prepublication dispatch.
    #[error(
        "full-progress snapshot after-effect generation {accepted} is not newer than prepublication dispatch generation {previous}"
    )]
    AfterEffectGenerationNotAfterPrePublish { accepted: u64, previous: u64 },
    /// A progress row key differs from the frozen plan.
    #[error("full-progress snapshot step {index} key {actual:?} does not match {expected:?}")]
    StepKeyMismatch {
        index: usize,
        actual: WorkUnitKey,
        expected: WorkUnitKey,
    },
    /// A progress row requirement differs from the frozen plan.
    #[error("full-progress snapshot step {key:?} requirement does not match the frozen plan")]
    StepRequirementMismatch { key: WorkUnitKey },
    /// A row status is inconsistent with plan applicability.
    #[error(
        "full-progress snapshot step {key:?} status {status:?} is inconsistent with {applicability:?}"
    )]
    StepApplicabilityMismatch {
        key: WorkUnitKey,
        applicability: WorkUnitApplicability,
        status: ProgressStepStatus,
    },
    /// Deferred status was attached to a non-optional artifact.
    #[error("full-progress snapshot deferred status is invalid for {key:?}")]
    DeferredRoleInvalid { key: WorkUnitKey },
    /// Retained completion evidence, context, or historical fence validation failed.
    #[error("invalid full-progress snapshot completion history: {source}")]
    Completion {
        #[source]
        source: WorkProgressError,
    },
    /// A completed row's direct prerequisite was absent.
    #[error("full-progress snapshot prerequisite {prerequisite:?} is missing for {dependent:?}")]
    PrerequisiteMissing {
        dependent: WorkUnitKey,
        prerequisite: WorkUnitKey,
    },
    /// A completed row's direct prerequisite was not complete.
    #[error("full-progress snapshot prerequisite {prerequisite:?} is incomplete for {dependent:?}")]
    PrerequisiteIncomplete {
        dependent: WorkUnitKey,
        prerequisite: WorkUnitKey,
    },
    /// A prerequisite completion was accepted after its dependent completion.
    #[error(
        "full-progress snapshot prerequisite {prerequisite:?} epoch {prerequisite_epoch} is newer than dependent {dependent:?} epoch {dependent_epoch}"
    )]
    PrerequisiteEpochInversion {
        dependent: WorkUnitKey,
        prerequisite: WorkUnitKey,
        prerequisite_epoch: u64,
        dependent_epoch: u64,
    },
    /// The candidate steps differ from the exact current progress rows.
    #[error("full-progress snapshot steps do not match current progress")]
    CurrentStepsMismatch,
    /// A mutable observation was omitted or changed.
    #[error("full-progress snapshot mutable field {field} does not match current progress")]
    MutableStateMismatch { field: &'static str },
}

impl WorkProgress {
    /// Capture the exact mutable progress observations tied to a validated immutable intent.
    ///
    /// The supplied intent is retained exactly once in the returned observation.  This method
    /// performs no mutation, ID generation, capacity reservation, persistence, or authority
    /// issuance.  Validation is intentionally limited to the snapshot base boundary described
    /// by [`FullProgressSnapshotObservation::validate_base`].
    pub fn capture_full_progress_snapshot(
        &self,
        intent: &FullProgressIntentObservation,
    ) -> Result<FullProgressSnapshotObservation, FullProgressSnapshotObservationError> {
        let maximum = self.plan.units().len();
        if self.readiness_deferral_batches.len() > maximum {
            return Err(
                FullProgressSnapshotObservationError::TooManyReadinessDeferralBatches {
                    actual: self.readiness_deferral_batches.len(),
                    maximum,
                },
            );
        }
        let retained_keys = self
            .readiness_deferral_batches
            .iter()
            .map(|batch| batch.artifact_keys().len())
            .fold(0usize, |total, count| total.saturating_add(count));
        let retained_rows = self
            .readiness_deferral_batches
            .iter()
            .map(|batch| batch.rows().len())
            .fold(0usize, |total, count| total.saturating_add(count));
        if retained_keys > maximum || retained_rows > maximum {
            return Err(
                FullProgressSnapshotObservationError::TooManyReadinessDeferralObservations {
                    keys: retained_keys,
                    rows: retained_rows,
                    maximum,
                },
            );
        }
        let WorkProgress {
            operation_id: _,
            plan: _,
            scopes,
            steps,
            phase,
            disposition,
            retry_observation,
            last_retry_observation,
            execution_epoch,
            active_audit,
            audit_generation,
            cancellation_request,
            admission_claims: _,
            claim_settlement,
            cancellation_inventory,
            origin_policies: _,
            recovery_generation,
            pre_publish_cancellation,
            after_effect_reconciliation,
            cancellation_cleanup_observations,
            readiness_deferral_batches,
        } = self;
        let snapshot = FullProgressSnapshotObservation {
            intent: intent.clone(),
            scopes: scopes.clone(),
            steps: steps.clone(),
            phase: *phase,
            disposition: *disposition,
            retry_observation: retry_observation.clone(),
            last_retry_observation: last_retry_observation.clone(),
            execution_epoch: *execution_epoch,
            active_audit: active_audit.clone(),
            audit_generation: *audit_generation,
            cancellation_request: *cancellation_request,
            claim_settlement: claim_settlement.clone(),
            cancellation_inventory: cancellation_inventory.clone(),
            recovery_generation: *recovery_generation,
            pre_publish_cancellation: pre_publish_cancellation.clone(),
            after_effect_reconciliation: after_effect_reconciliation.clone(),
            cancellation_cleanup_observations: cancellation_cleanup_observations.clone(),
            readiness_deferral_batches: readiness_deferral_batches.clone(),
        };
        snapshot.validate_base()?;
        snapshot.validate_against_progress(self)?;
        Ok(snapshot)
    }
}
