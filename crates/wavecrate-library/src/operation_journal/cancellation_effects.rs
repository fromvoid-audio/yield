//! Bounded observations of cancellation effects recorded by a frozen V1 plan.
//!
//! This module derives local effect recipes from the exact cancellation inventory and work plan.
//! It does not issue recovery authority, inspect the filesystem, mutate a source database, or
//! emit a terminal cancellation outcome.  Bundles record completed effect facts and marker
//! observations plus exact nearby source-closure statuses.  They do not prove that every entered
//! work unit is represented; preparation, staging, and recovery-only obligations remain in the
//! frozen cancellation steps for a future recovery owner.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

use super::admission_claims::{AdmissionClaimId, AdmissionClaimInventory, MAX_ADMISSION_CLAIMS};
use super::cancellation_dispatch::{
    CancellationDispatchManifestError, CancellationDispatchOutcome,
    CancellationDispatchOutcomeRecord, CancellationDispatchOutcomeRecordError,
    derive_cancellation_dispatch_manifest,
};
use super::cancellation_recovery::{
    CancellationInventory, CancellationInventoryError, MAX_CANCELLATION_ACTIONS,
};
use super::full_capacity::FullCapacityAllocationClass;
use super::model::OperationId;
use super::work_plan::{
    GLOBAL_ACTION_ORDINAL, STEP_HISTORY_STRIDE, WORK_PLAN_VERSION_V1, WorkPlan, WorkPrerequisite,
    WorkUnitApplicability, WorkUnitKey, WorkUnitRole,
};
use super::work_progress::{ProgressStepStatus, WorkProgressStep};

/// Maximum number of effect bundles retained for one cancellation inventory.
pub const MAX_CANCELLATION_EFFECT_BUNDLES: usize = MAX_CANCELLATION_ACTIONS;
/// Maximum number of completed facts and source-closure rows retained in one inventory.
pub const MAX_CANCELLATION_EFFECT_ROWS: usize = super::work_plan::MAX_WORK_PLAN_UNITS;

/// Recipe recognized from one exact action and V1 step segment.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum CancellationEffectRecipe {
    /// A staged filesystem publication without an origin-removal tail.
    StagedPublish,
    /// A staged publication followed by destination and origin source closure.
    StagedMove,
    /// A qualified no-replace relocation, including retained or history-folder relocation.
    QualifiedRelocation,
    /// A retained-folder merge, with an optional recorded publication completion.
    RetainedMerge,
    /// A selected retained-object disposition or purge.
    RetainedDisposition,
    /// An externally audited filesystem effect.
    ExternalAudit,
    /// The extracted origin-removal segment of a history undo restore.
    HistoryUndoExtractedRemoval,
}

/// Effect outcome retained for one completed local recipe.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum CancellationEffectOutcome {
    /// A publication completed and no origin-removal recipe is attached.
    Published,
    /// A staged move published, but no completed origin-removal marker proves the origin boundary.
    OriginUnverified,
    /// An origin-removal marker completed, but origin absence is not complete.  A qualified
    /// relocation may still be pending; source-closure status is retained separately.
    RemovalUnresolved,
    /// Origin absence completed.  Source-closure or reconciliation status is retained separately
    /// and is not required for this outcome.
    RemovalConfirmed,
    /// A qualified relocation completed its relocation checkpoint.
    QualifiedRelocationCompleted,
    /// A retained merge completed, whether or not its optional publication checkpoint completed.
    RetainedMergeCompleted,
    /// A retained-object disposition completed while absence remains unresolved.
    RetainedDispositionUnresolved,
    /// A retained-object disposition and its absence check completed.
    RetainedDispositionConfirmed,
    /// An external audit completed.
    ExternalAuditCompleted,
}

/// Kind of completed effect fact retained in a bundle.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum CancellationEffectFactKind {
    /// The plan's exact `Publish` checkpoint completed.
    Publish,
    /// The plan's exact `QualifiedRelocation` checkpoint completed.
    QualifiedRelocation,
    /// The plan's exact `RetainedMerge` checkpoint completed.
    RetainedMerge,
    /// The plan's exact selected retained-object disposition completed.
    SelectedRetainedObjectDisposition,
    /// The plan's exact `ExternalAudit` checkpoint completed.
    ExternalAudit,
    /// The plan's exact `OriginAbsence` checkpoint completed.
    OriginAbsence,
    /// The plan's exact `RetainedObjectAbsence` checkpoint completed.
    RetainedObjectAbsence,
    /// The plan's exact `OriginRemovalStart` marker completed.
    OriginRemovalStart,
}

impl CancellationEffectFactKind {
    const fn role(self) -> WorkUnitRole {
        match self {
            Self::Publish => WorkUnitRole::Publish,
            Self::QualifiedRelocation => WorkUnitRole::QualifiedRelocation,
            Self::RetainedMerge => WorkUnitRole::RetainedMerge,
            Self::SelectedRetainedObjectDisposition => {
                WorkUnitRole::SelectedRetainedObjectDisposition
            }
            Self::ExternalAudit => WorkUnitRole::ExternalAudit,
            Self::OriginAbsence => WorkUnitRole::OriginAbsence,
            Self::RetainedObjectAbsence => WorkUnitRole::RetainedObjectAbsence,
            Self::OriginRemovalStart => WorkUnitRole::OriginRemovalStart,
        }
    }
}

/// One exact completed effect checkpoint.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CancellationEffectFact {
    key: WorkUnitKey,
    kind: CancellationEffectFactKind,
}

impl CancellationEffectFact {
    /// Construct one typed completed fact.
    pub const fn new(key: WorkUnitKey, kind: CancellationEffectFactKind) -> Self {
        Self { key, kind }
    }

    /// Return the exact completed work-unit key.
    pub const fn key(self) -> WorkUnitKey {
        self.key
    }

    /// Return the typed fact kind.
    pub const fn kind(self) -> CancellationEffectFactKind {
        self.kind
    }
}

/// One exact source-closure key and its observed normal progress status.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CancellationEffectSourceClosure {
    key: WorkUnitKey,
    status: ProgressStepStatus,
}

impl CancellationEffectSourceClosure {
    /// Construct one source-closure observation without issuing a source receipt.
    pub const fn new(key: WorkUnitKey, status: ProgressStepStatus) -> Self {
        Self { key, status }
    }

    /// Return the exact source-closure work-unit key.
    pub const fn key(self) -> WorkUnitKey {
        self.key
    }

    /// Return the observed normal progress status.
    pub const fn status(self) -> ProgressStepStatus {
        self.status
    }
}

/// One bounded local effect bundle for an action and V1 step segment.
///
/// A completed origin-removal marker is itself retained when its associated relocation is still
/// pending.  Preparation-only, staging-only, and recovery-only segments intentionally produce no
/// effect bundle; their cleanup or in-flight obligations remain in the frozen cancellation steps.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CancellationEffectBundle {
    action_ordinal: u8,
    step_segment: u16,
    recipe: CancellationEffectRecipe,
    outcome: CancellationEffectOutcome,
    facts: Vec<CancellationEffectFact>,
    source_closure: Vec<CancellationEffectSourceClosure>,
}

impl CancellationEffectBundle {
    /// Construct one bounded effect bundle after checking its local row shape.
    pub fn new(
        action_ordinal: u8,
        step_segment: u16,
        recipe: CancellationEffectRecipe,
        outcome: CancellationEffectOutcome,
        facts: Vec<CancellationEffectFact>,
        source_closure: Vec<CancellationEffectSourceClosure>,
    ) -> Result<Self, CancellationEffectInventoryValidationError> {
        let bundle = Self {
            action_ordinal,
            step_segment,
            recipe,
            outcome,
            facts,
            source_closure,
        };
        bundle.validate()?;
        Ok(bundle)
    }

    /// Validate local ordering, fact typing, and row bounds.
    pub fn validate(&self) -> Result<(), CancellationEffectInventoryValidationError> {
        let row_count = self.facts.len().saturating_add(self.source_closure.len());
        if row_count > MAX_CANCELLATION_EFFECT_ROWS {
            return Err(CancellationEffectInventoryValidationError::TooManyRows {
                actual: row_count,
                maximum: MAX_CANCELLATION_EFFECT_ROWS,
            });
        }
        for (index, fact) in self.facts.iter().enumerate() {
            if fact.kind.role() != fact.key.role() {
                return Err(
                    CancellationEffectInventoryValidationError::FactKindMismatch {
                        action_ordinal: self.action_ordinal,
                        step_segment: self.step_segment,
                        index,
                    },
                );
            }
            if index > 0 && self.facts[index - 1].key() >= fact.key() {
                return Err(
                    CancellationEffectInventoryValidationError::FactsNotSortedOrUnique {
                        action_ordinal: self.action_ordinal,
                        step_segment: self.step_segment,
                        index,
                    },
                );
            }
        }
        for (index, closure) in self.source_closure.iter().enumerate() {
            if index > 0 && self.source_closure[index - 1].key() >= closure.key() {
                return Err(
                    CancellationEffectInventoryValidationError::SourceClosureNotSortedOrUnique {
                        action_ordinal: self.action_ordinal,
                        step_segment: self.step_segment,
                        index,
                    },
                );
            }
        }
        Ok(())
    }

    /// Return the frozen action ordinal.
    pub const fn action_ordinal(&self) -> u8 {
        self.action_ordinal
    }

    /// Return the V1 step segment within the action.
    pub const fn step_segment(&self) -> u16 {
        self.step_segment
    }

    /// Return the recognized local recipe.
    pub const fn recipe(&self) -> CancellationEffectRecipe {
        self.recipe
    }

    /// Return the retained local outcome.
    pub const fn outcome(&self) -> CancellationEffectOutcome {
        self.outcome
    }

    /// Borrow all completed effect facts in exact key order.
    pub fn facts(&self) -> &[CancellationEffectFact] {
        &self.facts
    }

    /// Borrow all local source-closure observations in exact key order.
    pub fn source_closure(&self) -> &[CancellationEffectSourceClosure] {
        &self.source_closure
    }
}

/// Bounded exact effect inventory derived from one frozen cancellation snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CancellationEffectInventory {
    operation_id: OperationId,
    plan_version: u16,
    frozen_inventory_generation: u64,
    capture_epoch: u64,
    bundles: Vec<CancellationEffectBundle>,
}

/// Bounded after-effect recovery observations derived from one frozen cancellation snapshot and
/// one validated cancellation-wide dispatch outcome record.
///
/// The frozen inventory and dispatch record are retained in full even when a refined derivation
/// has no bundle.  Only exact `ObservedAfterCancellation` rows overlay a temporary `Completed`
/// status while deriving bundles; `NeverEntered` and `EntryUncertain` remain pending observations.
/// This value is observational only and does not grant recovery, filesystem, source, cleanup, or
/// terminal authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancellationEffectRecoveryInventory {
    frozen_inventory: CancellationInventory,
    dispatch_outcome: CancellationDispatchOutcomeRecord,
    bundles: Vec<CancellationEffectBundle>,
}

/// Maximum number of local cancellation-cleanup segments retained for one frozen plan.
pub const MAX_CANCELLATION_CLEANUP_SEGMENTS: usize = MAX_CANCELLATION_EFFECT_ROWS;
/// Maximum number of local work-unit keys retained by one cleanup manifest.
pub const MAX_CANCELLATION_CLEANUP_KEYS: usize = MAX_CANCELLATION_EFFECT_ROWS;

/// One exact non-global recipe segment retained for cancellation cleanup accounting.
///
/// The complete `keys` list includes preparation, source-closure, and not-applicable roles.  The
/// `applicable_keys` list is derived from the planner's `Required` applicability and is only a
/// convenience view; neither list proves that a cleanup action was never entered, adopted, or
/// completed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancellationCleanupSegment {
    action_ordinal: u8,
    step_segment: u16,
    recipe: CancellationEffectRecipe,
    keys: Vec<WorkUnitKey>,
    applicable_keys: Vec<WorkUnitKey>,
}

impl CancellationCleanupSegment {
    /// Construct one observational segment candidate without granting cleanup authority.
    pub fn new(
        action_ordinal: u8,
        step_segment: u16,
        recipe: CancellationEffectRecipe,
        keys: Vec<WorkUnitKey>,
        applicable_keys: Vec<WorkUnitKey>,
    ) -> Result<Self, CancellationCleanupManifestValidationError> {
        let segment = Self {
            action_ordinal,
            step_segment,
            recipe,
            keys,
            applicable_keys,
        };
        segment.validate()?;
        Ok(segment)
    }

    /// Validate local key ordering and segment binding without consulting progress authority.
    pub fn validate(&self) -> Result<(), CancellationCleanupManifestValidationError> {
        if self.keys.is_empty() {
            return Err(CancellationCleanupManifestValidationError::EmptySegment {
                action_ordinal: self.action_ordinal,
                step_segment: self.step_segment,
            });
        }
        if self.keys.len() > MAX_CANCELLATION_CLEANUP_KEYS {
            return Err(CancellationCleanupManifestValidationError::TooManyKeys {
                action_ordinal: self.action_ordinal,
                step_segment: self.step_segment,
                actual: self.keys.len(),
                maximum: MAX_CANCELLATION_CLEANUP_KEYS,
            });
        }
        validate_segment_keys(self.action_ordinal, self.step_segment, &self.keys, false)?;
        validate_segment_keys(
            self.action_ordinal,
            self.step_segment,
            &self.applicable_keys,
            true,
        )?;
        for key in &self.applicable_keys {
            if !self.keys.contains(key) {
                return Err(
                    CancellationCleanupManifestValidationError::ApplicableKeyMissing {
                        action_ordinal: self.action_ordinal,
                        step_segment: self.step_segment,
                        key: *key,
                    },
                );
            }
        }
        Ok(())
    }

    /// Return the containing action ordinal.
    pub const fn action_ordinal(&self) -> u8 {
        self.action_ordinal
    }

    /// Return the V1 history segment within the action.
    pub const fn step_segment(&self) -> u16 {
        self.step_segment
    }

    /// Return the exact recognized local recipe.
    pub const fn recipe(&self) -> CancellationEffectRecipe {
        self.recipe
    }

    /// Borrow all local work-unit keys, including preparation, source closure, and N/A roles.
    pub fn keys(&self) -> &[WorkUnitKey] {
        &self.keys
    }

    /// Borrow the planner-derived required keys for this segment.
    pub fn applicable_keys(&self) -> &[WorkUnitKey] {
        &self.applicable_keys
    }

    /// Return the required key for one role, when the planner marked that role applicable.
    pub fn applicable_key(&self, role: WorkUnitRole) -> Option<WorkUnitKey> {
        self.applicable_keys
            .iter()
            .copied()
            .find(|key| key.role() == role)
    }

    /// Return the applicable staging key, when present.
    pub fn applicable_staging_key(&self) -> Option<WorkUnitKey> {
        self.applicable_key(WorkUnitRole::Stage)
    }

    /// Return the applicable recovery-material key, when present.
    pub fn applicable_recovery_material_key(&self) -> Option<WorkUnitKey> {
        self.applicable_key(WorkUnitRole::RecoveryMaterialRetained)
    }

    /// Return the applicable restore-record key, when present.
    pub fn applicable_restore_record_key(&self) -> Option<WorkUnitKey> {
        self.applicable_key(WorkUnitRole::RestoreRecordPrepared)
    }

    /// Return the applicable origin-removal marker key, when present.
    pub fn applicable_origin_removal_start_key(&self) -> Option<WorkUnitKey> {
        self.applicable_key(WorkUnitRole::OriginRemovalStart)
    }

    /// Return the applicable origin-absence key, when present.
    pub fn applicable_origin_absence_key(&self) -> Option<WorkUnitKey> {
        self.applicable_key(WorkUnitRole::OriginAbsence)
    }

    /// Return the applicable origin-reconciliation key, when present.
    pub fn applicable_origin_source_reconcile_key(&self) -> Option<WorkUnitKey> {
        self.applicable_key(WorkUnitRole::OriginSourceReconcile)
    }

    /// Return the applicable selected-retained-object disposition key, when present.
    pub fn applicable_selected_retained_object_disposition_key(&self) -> Option<WorkUnitKey> {
        self.applicable_key(WorkUnitRole::SelectedRetainedObjectDisposition)
    }

    /// Return the applicable retained-object absence key, when present.
    pub fn applicable_retained_object_absence_key(&self) -> Option<WorkUnitKey> {
        self.applicable_key(WorkUnitRole::RetainedObjectAbsence)
    }
}

/// Bounded canonical cleanup obligations for one cancellation recovery observation.
///
/// This manifest retains the complete recovery lineage and separately lists operation-wide
/// recovery-payload claim identities.  It is observational only: membership, pending status, or
/// a missing local role never proves no entry, absence, adoption, release, or terminal completion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancellationCleanupManifest {
    recovery_inventory: CancellationEffectRecoveryInventory,
    segments: Vec<CancellationCleanupSegment>,
    operation_recovery_payload_claim_ids: Vec<AdmissionClaimId>,
}

impl CancellationCleanupManifest {
    /// Construct one bounded cleanup-obligation candidate without granting cleanup authority.
    pub fn new(
        recovery_inventory: CancellationEffectRecoveryInventory,
        dispatch_outcome: CancellationDispatchOutcomeRecord,
        segments: Vec<CancellationCleanupSegment>,
        operation_recovery_payload_claim_ids: Vec<AdmissionClaimId>,
    ) -> Result<Self, CancellationCleanupManifestValidationError> {
        if recovery_inventory.dispatch_outcome() != &dispatch_outcome {
            return Err(CancellationCleanupManifestValidationError::DispatchOutcomeMismatch);
        }
        let manifest = Self {
            recovery_inventory,
            segments,
            operation_recovery_payload_claim_ids,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    /// Validate bounded segment and operation-claim ordering without consulting live authority.
    pub fn validate(&self) -> Result<(), CancellationCleanupManifestValidationError> {
        if self.segments.len() > MAX_CANCELLATION_CLEANUP_SEGMENTS {
            return Err(
                CancellationCleanupManifestValidationError::TooManySegments {
                    actual: self.segments.len(),
                    maximum: MAX_CANCELLATION_CLEANUP_SEGMENTS,
                },
            );
        }
        let mut key_count = 0usize;
        for (index, segment) in self.segments.iter().enumerate() {
            segment.validate()?;
            key_count = key_count.saturating_add(segment.keys.len());
            if key_count > MAX_CANCELLATION_CLEANUP_KEYS {
                return Err(
                    CancellationCleanupManifestValidationError::TooManyTotalKeys {
                        actual: key_count,
                        maximum: MAX_CANCELLATION_CLEANUP_KEYS,
                    },
                );
            }
            if index > 0 {
                let previous = &self.segments[index - 1];
                if (previous.action_ordinal, previous.step_segment)
                    >= (segment.action_ordinal, segment.step_segment)
                {
                    return Err(
                        CancellationCleanupManifestValidationError::SegmentsNotSortedOrUnique {
                            index,
                        },
                    );
                }
            }
        }
        if self.operation_recovery_payload_claim_ids.len() > MAX_ADMISSION_CLAIMS {
            return Err(
                CancellationCleanupManifestValidationError::TooManyOperationRecoveryClaims {
                    actual: self.operation_recovery_payload_claim_ids.len(),
                    maximum: MAX_ADMISSION_CLAIMS,
                },
            );
        }
        for (index, claim_id) in self.operation_recovery_payload_claim_ids.iter().enumerate() {
            claim_id.validate().map_err(|_| {
                CancellationCleanupManifestValidationError::InvalidOperationRecoveryClaim { index }
            })?;
            if index > 0 && self.operation_recovery_payload_claim_ids[index - 1] >= *claim_id {
                return Err(
                    CancellationCleanupManifestValidationError::OperationRecoveryClaimsNotSortedOrUnique {
                        index,
                    },
                );
            }
        }
        Ok(())
    }

    /// Validate this candidate against the exact frozen recovery lineage and canonical plan view.
    pub fn validate_against(
        &self,
        operation_id: OperationId,
        plan: &WorkPlan,
        expected_recovery_inventory: &CancellationEffectRecoveryInventory,
        expected_dispatch_outcome: &CancellationDispatchOutcomeRecord,
        current_steps: &[WorkProgressStep],
        admission_claims: &AdmissionClaimInventory,
        execution_epoch: u64,
    ) -> Result<(), CancellationCleanupManifestError> {
        self.validate()?;
        expected_recovery_inventory.validate_against(
            operation_id,
            plan,
            expected_recovery_inventory.frozen_inventory(),
            expected_dispatch_outcome,
            current_steps,
            admission_claims,
            execution_epoch,
        )?;
        if self.recovery_inventory != *expected_recovery_inventory {
            return Err(CancellationCleanupManifestError::RecoveryInventoryMismatch);
        }
        let expected = derive_cancellation_cleanup_manifest(
            operation_id,
            plan,
            expected_recovery_inventory,
            expected_dispatch_outcome,
            current_steps,
            admission_claims,
            execution_epoch,
        )?;
        if self != &expected {
            return Err(CancellationCleanupManifestError::CandidateMismatch);
        }
        Ok(())
    }

    /// Borrow the complete validated recovery inventory lineage.
    pub fn recovery_inventory(&self) -> &CancellationEffectRecoveryInventory {
        &self.recovery_inventory
    }

    /// Borrow the exact dispatch outcome lineage.
    pub fn dispatch_outcome(&self) -> &CancellationDispatchOutcomeRecord {
        self.recovery_inventory.dispatch_outcome()
    }

    /// Borrow canonical local cleanup segments in action and history-segment order.
    pub fn segments(&self) -> &[CancellationCleanupSegment] {
        &self.segments
    }

    /// Borrow operation-wide recovery-payload claim identities without segment mapping.
    pub fn operation_recovery_payload_claim_ids(&self) -> &[AdmissionClaimId] {
        &self.operation_recovery_payload_claim_ids
    }
}

/// Failure while validating one cleanup segment or manifest shape.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CancellationCleanupManifestValidationError {
    /// The explicit dispatch record differed from the recovery inventory lineage.
    #[error("cancellation cleanup dispatch outcome does not match recovery inventory")]
    DispatchOutcomeMismatch,
    /// A segment did not retain any local work-unit keys.
    #[error("cancellation cleanup segment {action_ordinal}/{step_segment} is empty")]
    EmptySegment {
        /// Containing action ordinal.
        action_ordinal: u8,
        /// V1 segment ordinal.
        step_segment: u16,
    },
    /// A segment retained more local keys than the frozen-plan bound.
    #[error(
        "cancellation cleanup segment {action_ordinal}/{step_segment} has {actual} keys, maximum is {maximum}"
    )]
    TooManyKeys {
        /// Containing action ordinal.
        action_ordinal: u8,
        /// V1 segment ordinal.
        step_segment: u16,
        /// Supplied key count.
        actual: usize,
        /// Maximum accepted key count.
        maximum: usize,
    },
    /// Segment keys were not strictly sorted and unique.
    #[error(
        "cancellation cleanup {kind} keys are not sorted and unique for segment {action_ordinal}/{step_segment} at index {index}"
    )]
    KeysNotSortedOrUnique {
        /// Containing action ordinal.
        action_ordinal: u8,
        /// V1 segment ordinal.
        step_segment: u16,
        /// Index at which ordering failed.
        index: usize,
        /// Whether the applicable subset was being checked.
        kind: &'static str,
    },
    /// A segment key did not belong to the declared action and history segment.
    #[error(
        "cancellation cleanup key {key:?} does not belong to segment {action_ordinal}/{step_segment}"
    )]
    KeySegmentMismatch {
        /// Containing action ordinal.
        action_ordinal: u8,
        /// V1 segment ordinal.
        step_segment: u16,
        /// Misbound local key.
        key: WorkUnitKey,
    },
    /// An applicable key was absent from the complete local key set.
    #[error(
        "cancellation cleanup applicable key {key:?} is absent from segment {action_ordinal}/{step_segment}"
    )]
    ApplicableKeyMissing {
        /// Containing action ordinal.
        action_ordinal: u8,
        /// V1 segment ordinal.
        step_segment: u16,
        /// Missing complete key.
        key: WorkUnitKey,
    },
    /// The segment count exceeded the frozen-plan bound.
    #[error("cancellation cleanup manifest has {actual} segments, maximum is {maximum}")]
    TooManySegments {
        /// Supplied segment count.
        actual: usize,
        /// Maximum accepted segment count.
        maximum: usize,
    },
    /// The manifest retained too many aggregate local keys.
    #[error("cancellation cleanup manifest has {actual} keys, maximum is {maximum}")]
    TooManyTotalKeys {
        /// Aggregate key count.
        actual: usize,
        /// Maximum accepted key count.
        maximum: usize,
    },
    /// Segments were not strictly sorted and unique.
    #[error("cancellation cleanup segments are not sorted and unique at index {index}")]
    SegmentsNotSortedOrUnique {
        /// Index at which ordering failed.
        index: usize,
    },
    /// The manifest retained too many operation-wide recovery claims.
    #[error(
        "cancellation cleanup manifest has {actual} operation-recovery claims, maximum is {maximum}"
    )]
    TooManyOperationRecoveryClaims {
        /// Supplied claim count.
        actual: usize,
        /// Maximum accepted claim count.
        maximum: usize,
    },
    /// An operation-recovery claim identity was invalid.
    #[error("cancellation cleanup operation-recovery claim at index {index} is invalid")]
    InvalidOperationRecoveryClaim {
        /// Invalid claim index.
        index: usize,
    },
    /// Operation-recovery claim identities were not strictly sorted and unique.
    #[error(
        "cancellation cleanup operation-recovery claims are not sorted and unique at index {index}"
    )]
    OperationRecoveryClaimsNotSortedOrUnique {
        /// Index at which ordering failed.
        index: usize,
    },
}

/// Failure while deriving or binding a canonical cleanup-obligation manifest.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum CancellationCleanupManifestError {
    /// The bound recovery inventory failed exact validation.
    #[error("invalid cancellation cleanup recovery inventory: {0}")]
    RecoveryInventory(#[source] CancellationEffectInventoryError),
    /// The bound recovery inventory did not retain the explicit dispatch record.
    #[error("cancellation cleanup dispatch outcome does not match recovery inventory")]
    DispatchOutcomeMismatch,
    /// The plan was not the supported frozen V1 plan.
    #[error("unsupported cancellation cleanup plan version {actual}")]
    UnsupportedPlanVersion {
        /// Plan version found in the supplied plan.
        actual: u16,
    },
    /// A local segment or manifest shape was invalid.
    #[error("invalid cancellation cleanup manifest shape: {0}")]
    Shape(#[source] CancellationCleanupManifestValidationError),
    /// The candidate retained another recovery inventory.
    #[error("cancellation cleanup recovery inventory does not match")]
    RecoveryInventoryMismatch,
    /// The candidate rows did not match the canonical plan derivation.
    #[error("cancellation cleanup manifest candidate does not match the frozen derivation")]
    CandidateMismatch,
}

impl From<CancellationEffectInventoryError> for CancellationCleanupManifestError {
    fn from(error: CancellationEffectInventoryError) -> Self {
        Self::RecoveryInventory(error)
    }
}

impl From<CancellationCleanupManifestValidationError> for CancellationCleanupManifestError {
    fn from(error: CancellationCleanupManifestValidationError) -> Self {
        Self::Shape(error)
    }
}

/// Failure while validating local effect-inventory shape.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CancellationEffectInventoryValidationError {
    /// The bundle count exceeds the cancellation action bound.
    #[error("cancellation effect inventory has {actual} bundles, maximum is {maximum}")]
    TooManyBundles {
        /// Number of bundles supplied by the candidate.
        actual: usize,
        /// Maximum number of bundles accepted for one inventory.
        maximum: usize,
    },
    /// Completed facts and source-closure rows exceed the work-plan bound.
    #[error("cancellation effect inventory has {actual} rows, maximum is {maximum}")]
    TooManyRows {
        /// Number of fact and source-closure rows supplied by the candidate.
        actual: usize,
        /// Maximum number of rows accepted for one inventory.
        maximum: usize,
    },
    /// Bundles are not strictly ordered by action and step segment.
    #[error("cancellation effect bundles are not sorted or unique at index {index}")]
    BundlesNotSortedOrUnique {
        /// Index at which ordering or uniqueness failed.
        index: usize,
    },
    /// Facts in a bundle are not strictly ordered by exact key.
    #[error(
        "cancellation effect facts are not sorted or unique for action {action_ordinal}, segment {step_segment}, index {index}"
    )]
    FactsNotSortedOrUnique {
        /// Action containing the malformed bundle.
        action_ordinal: u8,
        /// V1 segment containing the malformed bundle.
        step_segment: u16,
        /// Index at which ordering or uniqueness failed.
        index: usize,
    },
    /// Source-closure rows in a bundle are not strictly ordered by exact key.
    #[error(
        "cancellation effect source closure is not sorted or unique for action {action_ordinal}, segment {step_segment}, index {index}"
    )]
    SourceClosureNotSortedOrUnique {
        /// Action containing the malformed bundle.
        action_ordinal: u8,
        /// V1 segment containing the malformed bundle.
        step_segment: u16,
        /// Index at which ordering or uniqueness failed.
        index: usize,
    },
    /// A fact kind does not agree with the role encoded in its exact key.
    #[error(
        "cancellation effect fact kind does not match its key for action {action_ordinal}, segment {step_segment}, index {index}"
    )]
    FactKindMismatch {
        /// Action containing the malformed bundle.
        action_ordinal: u8,
        /// V1 segment containing the malformed bundle.
        step_segment: u16,
        /// Index at which the fact role disagreed with its kind.
        index: usize,
    },
}

/// Failure while binding or deriving an exact cancellation effect inventory.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum CancellationEffectInventoryError {
    /// No frozen cancellation inventory was retained by progress.
    #[error("cancellation effect inventory is unavailable without a frozen cancellation inventory")]
    CancellationInventoryMissing,
    /// No exact admission-claim observation was retained by progress.
    #[error("cancellation effect inventory requires retained admission claims")]
    AdmissionClaimsMissing,
    /// The frozen cancellation inventory did not validate against current progress.
    #[error("invalid cancellation effect inventory source: {0}")]
    Inventory(#[source] CancellationInventoryError),
    /// The plan is not the supported frozen V1 plan.
    #[error("unsupported cancellation effect plan version {actual}")]
    UnsupportedPlanVersion {
        /// Version found in the supplied plan.
        actual: u16,
    },
    /// One action and segment did not match a known V1 local recipe.
    #[error(
        "unsupported cancellation effect recipe for action {action_ordinal}, step segment {step_segment}"
    )]
    UnsupportedPattern {
        /// Action containing the unsupported segment.
        action_ordinal: u8,
        /// V1 segment containing the unsupported segment.
        step_segment: u16,
    },
    /// Candidate operation identity did not match the frozen source.
    #[error("cancellation effect inventory operation identity does not match")]
    OperationIdMismatch,
    /// Candidate plan version did not match the frozen plan.
    #[error("cancellation effect inventory plan version does not match")]
    PlanVersionMismatch,
    /// Candidate frozen-inventory generation did not match the source inventory.
    #[error("cancellation effect inventory generation does not match")]
    FrozenInventoryGenerationMismatch,
    /// Candidate capture epoch did not match the source inventory.
    #[error("cancellation effect inventory capture epoch does not match")]
    CaptureEpochMismatch,
    /// Candidate local shape was invalid.
    #[error("invalid cancellation effect inventory shape: {0}")]
    Shape(#[source] CancellationEffectInventoryValidationError),
    /// Candidate rows did not exactly match the derived V1 effect inventory.
    #[error("cancellation effect inventory candidate does not match the frozen derivation")]
    CandidateMismatch,
    /// The candidate carried an invalid dispatch manifest binding.
    #[error("invalid cancellation effect recovery dispatch manifest: {0}")]
    DispatchManifest(#[source] CancellationDispatchManifestError),
    /// The candidate carried an invalid dispatch outcome binding.
    #[error("invalid cancellation effect recovery dispatch outcome: {0}")]
    DispatchOutcome(#[source] CancellationDispatchOutcomeRecordError),
    /// The candidate did not retain the exact frozen cancellation inventory.
    #[error("cancellation effect recovery frozen inventory does not match")]
    FrozenInventoryMismatch,
    /// The candidate did not retain the exact dispatch outcome record.
    #[error("cancellation effect recovery dispatch outcome does not match")]
    DispatchOutcomeMismatch,
}

impl From<CancellationInventoryError> for CancellationEffectInventoryError {
    fn from(error: CancellationInventoryError) -> Self {
        Self::Inventory(error)
    }
}

impl From<CancellationEffectInventoryValidationError> for CancellationEffectInventoryError {
    fn from(error: CancellationEffectInventoryValidationError) -> Self {
        Self::Shape(error)
    }
}

impl CancellationEffectInventory {
    /// Construct one bounded effect inventory.
    pub fn new(
        operation_id: OperationId,
        plan_version: u16,
        frozen_inventory_generation: u64,
        capture_epoch: u64,
        bundles: Vec<CancellationEffectBundle>,
    ) -> Result<Self, CancellationEffectInventoryValidationError> {
        let inventory = Self {
            operation_id,
            plan_version,
            frozen_inventory_generation,
            capture_epoch,
            bundles,
        };
        inventory.validate()?;
        Ok(inventory)
    }

    /// Validate bounded ordering and row totals without consulting owner authority.
    pub fn validate(&self) -> Result<(), CancellationEffectInventoryValidationError> {
        validate_bundles(&self.bundles)
    }

    /// Validate a candidate against the exact frozen plan, inventory, claims, and current steps.
    ///
    /// This is intended for a future decoder or durable owner.  It validates candidate bounds
    /// before deriving or retaining any expected rows, then requires exact bundle, fact, and
    /// source-closure equality.
    pub fn validate_against(
        &self,
        operation_id: OperationId,
        plan: &WorkPlan,
        inventory: &CancellationInventory,
        current_steps: &[WorkProgressStep],
        admission_claims: &AdmissionClaimInventory,
        execution_epoch: u64,
    ) -> Result<(), CancellationEffectInventoryError> {
        self.validate()?;
        if self.operation_id != operation_id {
            return Err(CancellationEffectInventoryError::OperationIdMismatch);
        }
        if self.plan_version != plan.plan_version() {
            return Err(CancellationEffectInventoryError::PlanVersionMismatch);
        }
        if self.frozen_inventory_generation != inventory.generation() {
            return Err(CancellationEffectInventoryError::FrozenInventoryGenerationMismatch);
        }
        if self.capture_epoch != inventory.execution_epoch() {
            return Err(CancellationEffectInventoryError::CaptureEpochMismatch);
        }
        let expected = derive_cancellation_effect_inventory(
            operation_id,
            plan,
            inventory,
            current_steps,
            admission_claims,
            execution_epoch,
        )?;
        if self != &expected {
            return Err(CancellationEffectInventoryError::CandidateMismatch);
        }
        Ok(())
    }

    /// Return the operation identity bound to the frozen source inventory.
    pub const fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    /// Return the frozen V1 plan version.
    pub const fn plan_version(&self) -> u16 {
        self.plan_version
    }

    /// Return the cancellation-inventory generation captured by this effect inventory.
    pub const fn frozen_inventory_generation(&self) -> u64 {
        self.frozen_inventory_generation
    }

    /// Return the cancellation-inventory capture epoch.
    pub const fn capture_epoch(&self) -> u64 {
        self.capture_epoch
    }

    /// Borrow effect bundles in exact action and V1 segment order.
    pub fn bundles(&self) -> &[CancellationEffectBundle] {
        &self.bundles
    }
}

impl CancellationEffectRecoveryInventory {
    /// Construct one bounded observational recovery inventory after validating its bundle rows.
    pub(crate) fn new(
        frozen_inventory: CancellationInventory,
        dispatch_outcome: CancellationDispatchOutcomeRecord,
        bundles: Vec<CancellationEffectBundle>,
    ) -> Result<Self, CancellationEffectInventoryValidationError> {
        validate_bundles(&bundles)?;
        Ok(Self {
            frozen_inventory,
            dispatch_outcome,
            bundles,
        })
    }

    /// Validate exact retained source values and canonical refined bundles against current
    /// progress.  This remains an observational candidate check and grants no authority.
    pub fn validate_against(
        &self,
        operation_id: OperationId,
        plan: &WorkPlan,
        expected_inventory: &CancellationInventory,
        expected_dispatch_outcome: &CancellationDispatchOutcomeRecord,
        current_steps: &[WorkProgressStep],
        admission_claims: &AdmissionClaimInventory,
        execution_epoch: u64,
    ) -> Result<(), CancellationEffectInventoryError> {
        validate_bundles(&self.bundles).map_err(CancellationEffectInventoryError::Shape)?;
        expected_inventory.validate_against(
            operation_id,
            plan,
            current_steps,
            execution_epoch,
            admission_claims,
        )?;
        if &self.frozen_inventory != expected_inventory {
            return Err(CancellationEffectInventoryError::FrozenInventoryMismatch);
        }
        let manifest = derive_cancellation_dispatch_manifest(
            operation_id,
            plan,
            expected_inventory,
            current_steps,
            admission_claims,
            execution_epoch,
        )
        .map_err(CancellationEffectInventoryError::DispatchManifest)?;
        expected_dispatch_outcome
            .validate_against(&manifest, execution_epoch)
            .map_err(CancellationEffectInventoryError::DispatchOutcome)?;
        if &self.dispatch_outcome != expected_dispatch_outcome {
            return Err(CancellationEffectInventoryError::DispatchOutcomeMismatch);
        }
        let expected_bundles =
            derive_recovery_bundles(plan, expected_inventory, expected_dispatch_outcome)?;
        if self.bundles != expected_bundles {
            return Err(CancellationEffectInventoryError::CandidateMismatch);
        }
        Ok(())
    }

    /// Borrow the exact frozen cancellation snapshot retained with this observation.
    pub fn frozen_inventory(&self) -> &CancellationInventory {
        &self.frozen_inventory
    }

    /// Borrow the exact dispatch outcome record retained with this observation.
    pub fn dispatch_outcome(&self) -> &CancellationDispatchOutcomeRecord {
        &self.dispatch_outcome
    }

    /// Borrow canonical refined effect bundles in action and V1 segment order.
    pub fn bundles(&self) -> &[CancellationEffectBundle] {
        &self.bundles
    }
}

fn validate_bundles(
    bundles: &[CancellationEffectBundle],
) -> Result<(), CancellationEffectInventoryValidationError> {
    if bundles.len() > MAX_CANCELLATION_EFFECT_BUNDLES {
        return Err(CancellationEffectInventoryValidationError::TooManyBundles {
            actual: bundles.len(),
            maximum: MAX_CANCELLATION_EFFECT_BUNDLES,
        });
    }
    let mut row_count = 0usize;
    for (index, bundle) in bundles.iter().enumerate() {
        bundle.validate()?;
        row_count = row_count
            .saturating_add(bundle.facts.len())
            .saturating_add(bundle.source_closure.len());
        if row_count > MAX_CANCELLATION_EFFECT_ROWS {
            return Err(CancellationEffectInventoryValidationError::TooManyRows {
                actual: row_count,
                maximum: MAX_CANCELLATION_EFFECT_ROWS,
            });
        }
        if index > 0 {
            let previous = &bundles[index - 1];
            if (previous.action_ordinal, previous.step_segment)
                >= (bundle.action_ordinal, bundle.step_segment)
            {
                return Err(
                    CancellationEffectInventoryValidationError::BundlesNotSortedOrUnique { index },
                );
            }
        }
    }
    Ok(())
}

fn validate_segment_keys(
    action_ordinal: u8,
    step_segment: u16,
    keys: &[WorkUnitKey],
    applicable: bool,
) -> Result<(), CancellationCleanupManifestValidationError> {
    for (index, key) in keys.iter().enumerate() {
        if key.action_ordinal() != action_ordinal
            || key.step_ordinal() / STEP_HISTORY_STRIDE != step_segment
        {
            return Err(
                CancellationCleanupManifestValidationError::KeySegmentMismatch {
                    action_ordinal,
                    step_segment,
                    key: *key,
                },
            );
        }
        if index > 0 && keys[index - 1] >= *key {
            return Err(
                CancellationCleanupManifestValidationError::KeysNotSortedOrUnique {
                    action_ordinal,
                    step_segment,
                    index,
                    kind: if applicable { "applicable" } else { "local" },
                },
            );
        }
    }
    Ok(())
}

/// Derive an exact effect inventory after validating the frozen cancellation source.
pub(crate) fn derive_cancellation_effect_inventory(
    operation_id: OperationId,
    plan: &WorkPlan,
    inventory: &CancellationInventory,
    current_steps: &[WorkProgressStep],
    admission_claims: &AdmissionClaimInventory,
    execution_epoch: u64,
) -> Result<CancellationEffectInventory, CancellationEffectInventoryError> {
    inventory.validate_against(
        operation_id,
        plan,
        current_steps,
        execution_epoch,
        admission_claims,
    )?;
    if plan.plan_version() != WORK_PLAN_VERSION_V1 {
        return Err(CancellationEffectInventoryError::UnsupportedPlanVersion {
            actual: plan.plan_version(),
        });
    }

    let bundles = derive_bundles(plan, inventory, &BTreeSet::new())?;
    CancellationEffectInventory::new(
        operation_id,
        plan.plan_version(),
        inventory.generation(),
        inventory.execution_epoch(),
        bundles,
    )
    .map_err(CancellationEffectInventoryError::Shape)
}

/// Derive an after-effect recovery inventory by overlaying only observed late completions on the
/// frozen local statuses.  The frozen inventory and outcome record are cloned unchanged.
pub(crate) fn derive_cancellation_effect_recovery_inventory(
    operation_id: OperationId,
    plan: &WorkPlan,
    inventory: &CancellationInventory,
    dispatch_outcome: &CancellationDispatchOutcomeRecord,
    current_steps: &[WorkProgressStep],
    admission_claims: &AdmissionClaimInventory,
    execution_epoch: u64,
) -> Result<CancellationEffectRecoveryInventory, CancellationEffectInventoryError> {
    inventory.validate_against(
        operation_id,
        plan,
        current_steps,
        execution_epoch,
        admission_claims,
    )?;
    let manifest = derive_cancellation_dispatch_manifest(
        operation_id,
        plan,
        inventory,
        current_steps,
        admission_claims,
        execution_epoch,
    )
    .map_err(CancellationEffectInventoryError::DispatchManifest)?;
    dispatch_outcome
        .validate_against(&manifest, execution_epoch)
        .map_err(CancellationEffectInventoryError::DispatchOutcome)?;
    let bundles = derive_recovery_bundles(plan, inventory, dispatch_outcome)?;
    CancellationEffectRecoveryInventory::new(inventory.clone(), dispatch_outcome.clone(), bundles)
        .map_err(CancellationEffectInventoryError::Shape)
}

fn derive_recovery_bundles(
    plan: &WorkPlan,
    inventory: &CancellationInventory,
    dispatch_outcome: &CancellationDispatchOutcomeRecord,
) -> Result<Vec<CancellationEffectBundle>, CancellationEffectInventoryError> {
    let late_completed = late_completed_keys(dispatch_outcome);
    derive_bundles(plan, inventory, &late_completed)
}

fn late_completed_keys(
    dispatch_outcome: &CancellationDispatchOutcomeRecord,
) -> BTreeSet<WorkUnitKey> {
    dispatch_outcome
        .rows()
        .iter()
        .filter_map(|row| {
            matches!(
                row.outcome(),
                CancellationDispatchOutcome::ObservedAfterCancellation { .. }
            )
            .then_some(row.key())
        })
        .collect()
}

/// Derive the exact cleanup-obligation manifest from an explicitly validated recovery lineage.
pub(crate) fn derive_cancellation_cleanup_manifest(
    operation_id: OperationId,
    plan: &WorkPlan,
    recovery_inventory: &CancellationEffectRecoveryInventory,
    expected_dispatch_outcome: &CancellationDispatchOutcomeRecord,
    current_steps: &[WorkProgressStep],
    admission_claims: &AdmissionClaimInventory,
    execution_epoch: u64,
) -> Result<CancellationCleanupManifest, CancellationCleanupManifestError> {
    if plan.plan_version() != WORK_PLAN_VERSION_V1 {
        return Err(CancellationCleanupManifestError::UnsupportedPlanVersion {
            actual: plan.plan_version(),
        });
    }
    if recovery_inventory.dispatch_outcome() != expected_dispatch_outcome {
        return Err(CancellationCleanupManifestError::DispatchOutcomeMismatch);
    }
    recovery_inventory
        .validate_against(
            operation_id,
            plan,
            recovery_inventory.frozen_inventory(),
            expected_dispatch_outcome,
            current_steps,
            admission_claims,
            execution_epoch,
        )
        .map_err(CancellationCleanupManifestError::RecoveryInventory)?;

    let late_completed = late_completed_keys(expected_dispatch_outcome);
    let mut segments = Vec::new();
    let groups = grouped_units(plan, recovery_inventory.frozen_inventory(), &late_completed);
    for ((action_ordinal, step_segment), units) in groups {
        if action_ordinal == GLOBAL_ACTION_ORDINAL {
            continue;
        }
        let by_role =
            local_role_map(&units).ok_or(CancellationCleanupManifestError::RecoveryInventory(
                CancellationEffectInventoryError::UnsupportedPattern {
                    action_ordinal,
                    step_segment,
                },
            ))?;
        let recipe = classify_recipe(plan, &units, &by_role, action_ordinal, step_segment)
            .map_err(CancellationCleanupManifestError::RecoveryInventory)?;
        let mut keys = units.iter().map(|unit| unit.key).collect::<Vec<_>>();
        keys.sort_unstable();
        let mut applicable_keys = units
            .iter()
            .filter(|unit| unit.applicability == WorkUnitApplicability::Required)
            .map(|unit| unit.key)
            .collect::<Vec<_>>();
        applicable_keys.sort_unstable();
        segments.push(
            CancellationCleanupSegment::new(
                action_ordinal,
                step_segment,
                recipe,
                keys,
                applicable_keys,
            )
            .map_err(CancellationCleanupManifestError::Shape)?,
        );
    }
    segments.sort_by_key(|segment| (segment.action_ordinal, segment.step_segment));

    let mut operation_recovery_payload_claim_ids = recovery_inventory
        .frozen_inventory()
        .admission_claims()
        .claims()
        .iter()
        .filter(|claim| {
            claim.allocation_class() == FullCapacityAllocationClass::OperationRecoveryPayload
        })
        .map(|claim| claim.claim_id())
        .collect::<Vec<_>>();
    operation_recovery_payload_claim_ids.sort_unstable();
    CancellationCleanupManifest::new(
        recovery_inventory.clone(),
        expected_dispatch_outcome.clone(),
        segments,
        operation_recovery_payload_claim_ids,
    )
    .map_err(CancellationCleanupManifestError::Shape)
}

fn derive_bundles(
    plan: &WorkPlan,
    inventory: &CancellationInventory,
    late_completed: &BTreeSet<WorkUnitKey>,
) -> Result<Vec<CancellationEffectBundle>, CancellationEffectInventoryError> {
    let groups = grouped_units(plan, inventory, late_completed);
    let mut bundles = Vec::new();
    for ((action_ordinal, step_segment), units) in groups {
        if action_ordinal == GLOBAL_ACTION_ORDINAL {
            continue;
        }
        let by_role =
            local_role_map(&units).ok_or(CancellationEffectInventoryError::UnsupportedPattern {
                action_ordinal,
                step_segment,
            })?;
        let recipe = classify_recipe(plan, &units, &by_role, action_ordinal, step_segment)?;
        if let Some(bundle) = derive_bundle(action_ordinal, step_segment, recipe, &units)? {
            bundles.push(bundle);
        }
    }
    bundles.sort_by_key(|bundle: &CancellationEffectBundle| {
        (bundle.action_ordinal, bundle.step_segment)
    });
    Ok(bundles)
}

#[derive(Clone, Copy)]
struct LocalUnit {
    key: WorkUnitKey,
    requirement: super::participants::ParticipantRequirement,
    applicability: WorkUnitApplicability,
    status: ProgressStepStatus,
}

fn grouped_units(
    plan: &WorkPlan,
    inventory: &CancellationInventory,
    late_completed: &BTreeSet<WorkUnitKey>,
) -> BTreeMap<(u8, u16), Vec<LocalUnit>> {
    let mut groups = BTreeMap::new();
    for (unit, snapshot) in plan.units().iter().zip(inventory.steps()) {
        let key = unit.key();
        groups
            .entry((
                key.action_ordinal(),
                key.step_ordinal() / STEP_HISTORY_STRIDE,
            ))
            .or_insert_with(Vec::new)
            .push(LocalUnit {
                key,
                requirement: unit.requirement(),
                applicability: unit.applicability(),
                status: if late_completed.contains(&key) {
                    ProgressStepStatus::Completed
                } else {
                    snapshot.status()
                },
            });
    }
    groups
}

fn local_role_map(units: &[LocalUnit]) -> Option<BTreeMap<WorkUnitRole, LocalUnit>> {
    let mut by_role = BTreeMap::new();
    for unit in units {
        if by_role.insert(unit.key.role(), *unit).is_some() {
            return None;
        }
    }
    Some(by_role)
}

fn matches_roles(
    by_role: &BTreeMap<WorkUnitRole, LocalUnit>,
    required: &[(WorkUnitRole, WorkUnitApplicability)],
    optional: &[(WorkUnitRole, WorkUnitApplicability)],
) -> bool {
    for (role, applicability) in required {
        let Some(unit) = by_role.get(role) else {
            return false;
        };
        if unit.requirement != super::participants::ParticipantRequirement::Required
            || unit.applicability != *applicability
        {
            return false;
        }
    }
    for (role, applicability) in optional {
        if let Some(unit) = by_role.get(role)
            && (unit.requirement != super::participants::ParticipantRequirement::Required
                || unit.applicability != *applicability)
        {
            return false;
        }
    }
    by_role.keys().all(|role| {
        required.iter().any(|(candidate, _)| candidate == role)
            || optional.iter().any(|(candidate, _)| candidate == role)
    })
}

fn exact_local_edges(
    plan: &WorkPlan,
    units: &[LocalUnit],
    by_role: &BTreeMap<WorkUnitRole, LocalUnit>,
    edges: &[(WorkUnitRole, WorkUnitRole)],
) -> bool {
    let group_keys = units.iter().map(|unit| unit.key).collect::<BTreeSet<_>>();
    let expected = edges
        .iter()
        .filter_map(|(from, to)| {
            Some(WorkPrerequisite::new(
                by_role.get(from)?.key,
                by_role.get(to)?.key,
            ))
        })
        .collect::<BTreeSet<_>>();
    if expected.len() != edges.len()
        || expected
            .iter()
            .any(|edge| !plan.prerequisites().contains(edge))
    {
        return false;
    }
    plan.prerequisites().iter().all(|edge| {
        !group_keys.contains(&edge.prerequisite())
            || !group_keys.contains(&edge.dependent())
            || expected.contains(edge)
    })
}

fn classify_recipe(
    plan: &WorkPlan,
    units: &[LocalUnit],
    by_role: &BTreeMap<WorkUnitRole, LocalUnit>,
    action_ordinal: u8,
    step_segment: u16,
) -> Result<CancellationEffectRecipe, CancellationEffectInventoryError> {
    let unsupported = || CancellationEffectInventoryError::UnsupportedPattern {
        action_ordinal,
        step_segment,
    };
    if matches_staged_move(plan, units, by_role) {
        return Ok(CancellationEffectRecipe::StagedMove);
    }
    if matches_history_undo_removal(plan, units, by_role) {
        return Ok(CancellationEffectRecipe::HistoryUndoExtractedRemoval);
    }
    if matches_qualified_relocation(plan, units, by_role) {
        return Ok(CancellationEffectRecipe::QualifiedRelocation);
    }
    if matches_retained_merge(plan, units, by_role) {
        return Ok(CancellationEffectRecipe::RetainedMerge);
    }
    if matches_retained_disposition(plan, units, by_role) {
        return Ok(CancellationEffectRecipe::RetainedDisposition);
    }
    if matches_external_audit(plan, units, by_role) {
        return Ok(CancellationEffectRecipe::ExternalAudit);
    }
    if matches_staged_publish(plan, units, by_role) {
        return Ok(CancellationEffectRecipe::StagedPublish);
    }
    Err(unsupported())
}

fn matches_staged_publish(
    plan: &WorkPlan,
    units: &[LocalUnit],
    by_role: &BTreeMap<WorkUnitRole, LocalUnit>,
) -> bool {
    let Some(stage) = by_role.get(&WorkUnitRole::Stage) else {
        return false;
    };
    let Some(copy_validated) = by_role.get(&WorkUnitRole::CopyValidated) else {
        return false;
    };
    if stage.applicability != copy_validated.applicability
        || !matches!(
            stage.applicability,
            WorkUnitApplicability::Required | WorkUnitApplicability::NotApplicable
        )
    {
        return false;
    }
    let required = [
        (WorkUnitRole::Prepare, WorkUnitApplicability::Required),
        (WorkUnitRole::Stage, stage.applicability),
        (WorkUnitRole::CopyValidated, copy_validated.applicability),
        (WorkUnitRole::Publish, WorkUnitApplicability::Required),
        (WorkUnitRole::SourceAffect, WorkUnitApplicability::Required),
    ];
    let optional = [
        (WorkUnitRole::SourceRead, WorkUnitApplicability::Required),
        (
            WorkUnitRole::RecoveryMaterialRetained,
            WorkUnitApplicability::Required,
        ),
    ];
    if !matches_roles(by_role, &required, &optional) {
        return false;
    }
    let source_read_present = by_role.contains_key(&WorkUnitRole::SourceRead);
    let recovery_present = by_role.contains_key(&WorkUnitRole::RecoveryMaterialRetained);
    if recovery_present && !source_read_present {
        return false;
    }
    if !source_read_present && stage.applicability != WorkUnitApplicability::Required {
        return false;
    }
    let mut edges = Vec::new();
    if source_read_present {
        if recovery_present {
            edges.push((
                WorkUnitRole::SourceRead,
                WorkUnitRole::RecoveryMaterialRetained,
            ));
            edges.push((
                WorkUnitRole::RecoveryMaterialRetained,
                WorkUnitRole::Prepare,
            ));
        } else {
            edges.push((WorkUnitRole::SourceRead, WorkUnitRole::Prepare));
        }
    }
    if stage.applicability == WorkUnitApplicability::Required {
        edges.push((WorkUnitRole::Prepare, WorkUnitRole::Stage));
        edges.push((WorkUnitRole::Stage, WorkUnitRole::CopyValidated));
        edges.push((WorkUnitRole::CopyValidated, WorkUnitRole::Publish));
    } else {
        edges.push((WorkUnitRole::Prepare, WorkUnitRole::Publish));
    }
    edges.push((WorkUnitRole::Publish, WorkUnitRole::SourceAffect));
    exact_local_edges(plan, units, by_role, &edges)
}

fn matches_staged_move(
    plan: &WorkPlan,
    units: &[LocalUnit],
    by_role: &BTreeMap<WorkUnitRole, LocalUnit>,
) -> bool {
    let required = [
        (WorkUnitRole::SourceRead, WorkUnitApplicability::Required),
        (WorkUnitRole::Prepare, WorkUnitApplicability::Required),
        (WorkUnitRole::Stage, WorkUnitApplicability::Required),
        (WorkUnitRole::CopyValidated, WorkUnitApplicability::Required),
        (WorkUnitRole::Publish, WorkUnitApplicability::Required),
        (
            WorkUnitRole::DestinationSourceReconcile,
            WorkUnitApplicability::Required,
        ),
        (
            WorkUnitRole::OriginRemovalStart,
            WorkUnitApplicability::Required,
        ),
        (WorkUnitRole::OriginAbsence, WorkUnitApplicability::Required),
        (
            WorkUnitRole::OriginSourceReconcile,
            WorkUnitApplicability::Required,
        ),
    ];
    if !matches_roles(by_role, &required, &[]) {
        return false;
    }
    exact_local_edges(
        plan,
        units,
        by_role,
        &[
            (WorkUnitRole::SourceRead, WorkUnitRole::Prepare),
            (WorkUnitRole::Prepare, WorkUnitRole::Stage),
            (WorkUnitRole::Stage, WorkUnitRole::CopyValidated),
            (WorkUnitRole::CopyValidated, WorkUnitRole::Publish),
            (
                WorkUnitRole::Publish,
                WorkUnitRole::DestinationSourceReconcile,
            ),
            (
                WorkUnitRole::DestinationSourceReconcile,
                WorkUnitRole::OriginRemovalStart,
            ),
            (
                WorkUnitRole::OriginRemovalStart,
                WorkUnitRole::OriginAbsence,
            ),
            (
                WorkUnitRole::OriginAbsence,
                WorkUnitRole::OriginSourceReconcile,
            ),
        ],
    )
}

fn matches_history_undo_removal(
    plan: &WorkPlan,
    units: &[LocalUnit],
    by_role: &BTreeMap<WorkUnitRole, LocalUnit>,
) -> bool {
    let required = [
        (
            WorkUnitRole::OriginRemovalStart,
            WorkUnitApplicability::Required,
        ),
        (WorkUnitRole::OriginAbsence, WorkUnitApplicability::Required),
        (
            WorkUnitRole::OriginSourceReconcile,
            WorkUnitApplicability::Required,
        ),
    ];
    matches_roles(by_role, &required, &[])
        && exact_local_edges(
            plan,
            units,
            by_role,
            &[
                (
                    WorkUnitRole::OriginRemovalStart,
                    WorkUnitRole::OriginAbsence,
                ),
                (
                    WorkUnitRole::OriginAbsence,
                    WorkUnitRole::OriginSourceReconcile,
                ),
            ],
        )
}

fn matches_qualified_relocation(
    plan: &WorkPlan,
    units: &[LocalUnit],
    by_role: &BTreeMap<WorkUnitRole, LocalUnit>,
) -> bool {
    let required = [
        (WorkUnitRole::SourceRead, WorkUnitApplicability::Required),
        (WorkUnitRole::Prepare, WorkUnitApplicability::Required),
        (WorkUnitRole::Stage, WorkUnitApplicability::NotApplicable),
        (
            WorkUnitRole::CopyValidated,
            WorkUnitApplicability::NotApplicable,
        ),
        (
            WorkUnitRole::QualifiedRelocation,
            WorkUnitApplicability::Required,
        ),
        (WorkUnitRole::OriginAbsence, WorkUnitApplicability::Required),
        (WorkUnitRole::SourceAffect, WorkUnitApplicability::Required),
    ];
    let optional = [
        (
            WorkUnitRole::RestoreRecordPrepared,
            WorkUnitApplicability::Required,
        ),
        (
            WorkUnitRole::EnsureContainedParentDirectories,
            WorkUnitApplicability::Required,
        ),
        (
            WorkUnitRole::OriginRemovalStart,
            WorkUnitApplicability::Required,
        ),
        (
            WorkUnitRole::RetainedPayloadVerified,
            WorkUnitApplicability::Required,
        ),
    ];
    if !matches_roles(by_role, &required, &optional) {
        return false;
    }
    let restore = by_role.contains_key(&WorkUnitRole::RestoreRecordPrepared);
    let ensure_parents = by_role.contains_key(&WorkUnitRole::EnsureContainedParentDirectories);
    let removal_start = by_role.contains_key(&WorkUnitRole::OriginRemovalStart);
    let retained_payload = by_role.contains_key(&WorkUnitRole::RetainedPayloadVerified);
    if restore != removal_start || removal_start != retained_payload {
        return false;
    }
    let mut edges = vec![(WorkUnitRole::SourceRead, WorkUnitRole::Prepare)];
    let mut predecessor = WorkUnitRole::Prepare;
    if restore {
        edges.push((predecessor, WorkUnitRole::RestoreRecordPrepared));
        predecessor = WorkUnitRole::RestoreRecordPrepared;
    }
    if ensure_parents {
        edges.push((predecessor, WorkUnitRole::EnsureContainedParentDirectories));
        predecessor = WorkUnitRole::EnsureContainedParentDirectories;
    }
    if removal_start {
        edges.push((predecessor, WorkUnitRole::OriginRemovalStart));
        predecessor = WorkUnitRole::OriginRemovalStart;
    }
    edges.push((predecessor, WorkUnitRole::QualifiedRelocation));
    predecessor = WorkUnitRole::QualifiedRelocation;
    if retained_payload {
        edges.push((predecessor, WorkUnitRole::RetainedPayloadVerified));
        predecessor = WorkUnitRole::RetainedPayloadVerified;
    }
    edges.push((predecessor, WorkUnitRole::OriginAbsence));
    edges.push((WorkUnitRole::OriginAbsence, WorkUnitRole::SourceAffect));
    exact_local_edges(plan, units, by_role, &edges)
}

fn matches_retained_merge(
    plan: &WorkPlan,
    units: &[LocalUnit],
    by_role: &BTreeMap<WorkUnitRole, LocalUnit>,
) -> bool {
    let required = [
        (WorkUnitRole::SourceRead, WorkUnitApplicability::Required),
        (WorkUnitRole::Prepare, WorkUnitApplicability::Required),
        (WorkUnitRole::Stage, WorkUnitApplicability::NotApplicable),
        (WorkUnitRole::RetainedMerge, WorkUnitApplicability::Required),
        (
            WorkUnitRole::CopyValidated,
            WorkUnitApplicability::NotApplicable,
        ),
        (WorkUnitRole::Publish, WorkUnitApplicability::Required),
        (WorkUnitRole::SourceAffect, WorkUnitApplicability::Required),
    ];
    matches_roles(by_role, &required, &[])
        && exact_local_edges(
            plan,
            units,
            by_role,
            &[
                (WorkUnitRole::SourceRead, WorkUnitRole::Prepare),
                (WorkUnitRole::Prepare, WorkUnitRole::RetainedMerge),
                (WorkUnitRole::RetainedMerge, WorkUnitRole::Publish),
                (WorkUnitRole::Publish, WorkUnitRole::SourceAffect),
            ],
        )
}

fn matches_retained_disposition(
    plan: &WorkPlan,
    units: &[LocalUnit],
    by_role: &BTreeMap<WorkUnitRole, LocalUnit>,
) -> bool {
    let required = [
        (WorkUnitRole::SourceRead, WorkUnitApplicability::Required),
        (WorkUnitRole::Prepare, WorkUnitApplicability::Required),
        (WorkUnitRole::Stage, WorkUnitApplicability::NotApplicable),
        (
            WorkUnitRole::CopyValidated,
            WorkUnitApplicability::NotApplicable,
        ),
        (
            WorkUnitRole::SelectedRetainedObjectDisposition,
            WorkUnitApplicability::Required,
        ),
        (
            WorkUnitRole::RetainedObjectAbsence,
            WorkUnitApplicability::Required,
        ),
        (WorkUnitRole::SourceAffect, WorkUnitApplicability::Required),
    ];
    matches_roles(by_role, &required, &[])
        && exact_local_edges(
            plan,
            units,
            by_role,
            &[
                (WorkUnitRole::SourceRead, WorkUnitRole::Prepare),
                (
                    WorkUnitRole::Prepare,
                    WorkUnitRole::SelectedRetainedObjectDisposition,
                ),
                (
                    WorkUnitRole::SelectedRetainedObjectDisposition,
                    WorkUnitRole::RetainedObjectAbsence,
                ),
                (
                    WorkUnitRole::RetainedObjectAbsence,
                    WorkUnitRole::SourceAffect,
                ),
            ],
        )
}

fn matches_external_audit(
    plan: &WorkPlan,
    units: &[LocalUnit],
    by_role: &BTreeMap<WorkUnitRole, LocalUnit>,
) -> bool {
    let required = [
        (WorkUnitRole::SourceRead, WorkUnitApplicability::Required),
        (WorkUnitRole::ExternalAudit, WorkUnitApplicability::Required),
        (WorkUnitRole::SourceAffect, WorkUnitApplicability::Required),
    ];
    matches_roles(by_role, &required, &[])
        && exact_local_edges(
            plan,
            units,
            by_role,
            &[
                (WorkUnitRole::SourceRead, WorkUnitRole::ExternalAudit),
                (WorkUnitRole::ExternalAudit, WorkUnitRole::SourceAffect),
            ],
        )
}

fn fact_kind_for(
    recipe: CancellationEffectRecipe,
    role: WorkUnitRole,
) -> Option<CancellationEffectFactKind> {
    match recipe {
        CancellationEffectRecipe::StagedPublish => {
            (role == WorkUnitRole::Publish).then_some(CancellationEffectFactKind::Publish)
        }
        CancellationEffectRecipe::StagedMove => match role {
            WorkUnitRole::Publish => Some(CancellationEffectFactKind::Publish),
            WorkUnitRole::OriginRemovalStart => {
                Some(CancellationEffectFactKind::OriginRemovalStart)
            }
            WorkUnitRole::OriginAbsence => Some(CancellationEffectFactKind::OriginAbsence),
            _ => None,
        },
        CancellationEffectRecipe::QualifiedRelocation => match role {
            WorkUnitRole::QualifiedRelocation => {
                Some(CancellationEffectFactKind::QualifiedRelocation)
            }
            WorkUnitRole::OriginRemovalStart => {
                Some(CancellationEffectFactKind::OriginRemovalStart)
            }
            WorkUnitRole::OriginAbsence => Some(CancellationEffectFactKind::OriginAbsence),
            _ => None,
        },
        CancellationEffectRecipe::RetainedMerge => match role {
            WorkUnitRole::RetainedMerge => Some(CancellationEffectFactKind::RetainedMerge),
            WorkUnitRole::Publish => Some(CancellationEffectFactKind::Publish),
            _ => None,
        },
        CancellationEffectRecipe::RetainedDisposition => match role {
            WorkUnitRole::SelectedRetainedObjectDisposition => {
                Some(CancellationEffectFactKind::SelectedRetainedObjectDisposition)
            }
            WorkUnitRole::RetainedObjectAbsence => {
                Some(CancellationEffectFactKind::RetainedObjectAbsence)
            }
            _ => None,
        },
        CancellationEffectRecipe::ExternalAudit => (role == WorkUnitRole::ExternalAudit)
            .then_some(CancellationEffectFactKind::ExternalAudit),
        CancellationEffectRecipe::HistoryUndoExtractedRemoval => match role {
            WorkUnitRole::OriginRemovalStart => {
                Some(CancellationEffectFactKind::OriginRemovalStart)
            }
            WorkUnitRole::OriginAbsence => Some(CancellationEffectFactKind::OriginAbsence),
            _ => None,
        },
    }
}

fn source_closure_roles(recipe: CancellationEffectRecipe) -> &'static [WorkUnitRole] {
    match recipe {
        CancellationEffectRecipe::StagedMove => &[
            WorkUnitRole::SourceRead,
            WorkUnitRole::DestinationSourceReconcile,
            WorkUnitRole::OriginSourceReconcile,
        ],
        CancellationEffectRecipe::HistoryUndoExtractedRemoval => {
            &[WorkUnitRole::OriginSourceReconcile]
        }
        _ => &[WorkUnitRole::SourceRead, WorkUnitRole::SourceAffect],
    }
}

fn status_for(units: &[LocalUnit], role: WorkUnitRole) -> Option<ProgressStepStatus> {
    units
        .iter()
        .find(|unit| unit.key.role() == role)
        .map(|unit| unit.status)
}

fn derive_outcome(
    recipe: CancellationEffectRecipe,
    units: &[LocalUnit],
) -> CancellationEffectOutcome {
    match recipe {
        CancellationEffectRecipe::StagedPublish => CancellationEffectOutcome::Published,
        CancellationEffectRecipe::StagedMove => {
            if status_for(units, WorkUnitRole::OriginRemovalStart)
                != Some(ProgressStepStatus::Completed)
            {
                CancellationEffectOutcome::OriginUnverified
            } else if status_for(units, WorkUnitRole::OriginAbsence)
                == Some(ProgressStepStatus::Completed)
            {
                CancellationEffectOutcome::RemovalConfirmed
            } else {
                CancellationEffectOutcome::RemovalUnresolved
            }
        }
        CancellationEffectRecipe::QualifiedRelocation => {
            if status_for(units, WorkUnitRole::QualifiedRelocation)
                == Some(ProgressStepStatus::Completed)
            {
                CancellationEffectOutcome::QualifiedRelocationCompleted
            } else {
                CancellationEffectOutcome::RemovalUnresolved
            }
        }
        CancellationEffectRecipe::RetainedMerge => {
            CancellationEffectOutcome::RetainedMergeCompleted
        }
        CancellationEffectRecipe::RetainedDisposition => {
            if status_for(units, WorkUnitRole::RetainedObjectAbsence)
                == Some(ProgressStepStatus::Completed)
            {
                CancellationEffectOutcome::RetainedDispositionConfirmed
            } else {
                CancellationEffectOutcome::RetainedDispositionUnresolved
            }
        }
        CancellationEffectRecipe::ExternalAudit => {
            CancellationEffectOutcome::ExternalAuditCompleted
        }
        CancellationEffectRecipe::HistoryUndoExtractedRemoval => {
            if status_for(units, WorkUnitRole::OriginAbsence) == Some(ProgressStepStatus::Completed)
            {
                CancellationEffectOutcome::RemovalConfirmed
            } else {
                CancellationEffectOutcome::RemovalUnresolved
            }
        }
    }
}

fn derive_bundle(
    action_ordinal: u8,
    step_segment: u16,
    recipe: CancellationEffectRecipe,
    units: &[LocalUnit],
) -> Result<Option<CancellationEffectBundle>, CancellationEffectInventoryError> {
    let facts = units
        .iter()
        .filter_map(|unit| {
            (unit.status == ProgressStepStatus::Completed)
                .then(|| {
                    fact_kind_for(recipe, unit.key.role())
                        .map(|kind| CancellationEffectFact::new(unit.key, kind))
                })
                .flatten()
        })
        .collect::<Vec<_>>();
    if facts.is_empty() {
        return Ok(None);
    }
    let closure_roles = source_closure_roles(recipe);
    let source_closure = units
        .iter()
        .filter(|unit| closure_roles.contains(&unit.key.role()))
        .map(|unit| CancellationEffectSourceClosure::new(unit.key, unit.status))
        .collect::<Vec<_>>();
    CancellationEffectBundle::new(
        action_ordinal,
        step_segment,
        recipe,
        derive_outcome(recipe, units),
        facts,
        source_closure,
    )
    .map(Some)
    .map_err(CancellationEffectInventoryError::Shape)
}

#[cfg(test)]
mod tests {
    use super::super::participants::{ParticipantId, ParticipantRequirement};
    use super::*;
    use uuid::Uuid;

    fn local_unit(role: WorkUnitRole, step: u16, status: ProgressStepStatus) -> LocalUnit {
        LocalUnit {
            key: WorkUnitKey::new(
                0,
                step,
                ParticipantId::from_uuid(Uuid::from_u128(u128::from(step) + 1)),
                role,
            ),
            requirement: ParticipantRequirement::Required,
            applicability: if matches!(role, WorkUnitRole::Stage | WorkUnitRole::CopyValidated) {
                WorkUnitApplicability::NotApplicable
            } else {
                WorkUnitApplicability::Required
            },
            status,
        }
    }

    fn completed(role: WorkUnitRole, step: u16) -> LocalUnit {
        local_unit(role, step, ProgressStepStatus::Completed)
    }

    fn pending(role: WorkUnitRole, step: u16) -> LocalUnit {
        local_unit(role, step, ProgressStepStatus::Pending)
    }

    #[test]
    fn absence_confirms_removal_without_source_reconcile() {
        let staged_move = vec![
            completed(WorkUnitRole::SourceRead, 10),
            completed(WorkUnitRole::Publish, 20),
            completed(WorkUnitRole::DestinationSourceReconcile, 30),
            completed(WorkUnitRole::OriginRemovalStart, 40),
            completed(WorkUnitRole::OriginAbsence, 50),
            pending(WorkUnitRole::OriginSourceReconcile, 60),
        ];
        let staged_bundle = derive_bundle(0, 0, CancellationEffectRecipe::StagedMove, &staged_move)
            .expect("staged-move derivation")
            .expect("staged-move bundle");
        assert_eq!(
            staged_bundle.outcome(),
            CancellationEffectOutcome::RemovalConfirmed
        );
        assert_eq!(
            staged_bundle
                .source_closure()
                .iter()
                .find(|row| row.key().role() == WorkUnitRole::OriginSourceReconcile)
                .expect("origin source closure")
                .status(),
            ProgressStepStatus::Pending
        );

        let history_undo = vec![
            completed(WorkUnitRole::OriginRemovalStart, 10),
            completed(WorkUnitRole::OriginAbsence, 20),
            pending(WorkUnitRole::OriginSourceReconcile, 30),
        ];
        let history_bundle = derive_bundle(
            0,
            1,
            CancellationEffectRecipe::HistoryUndoExtractedRemoval,
            &history_undo,
        )
        .expect("history derivation")
        .expect("history bundle");
        assert_eq!(
            history_bundle.outcome(),
            CancellationEffectOutcome::RemovalConfirmed
        );
    }

    #[test]
    fn qualified_relocation_marker_is_retained_while_relocation_is_pending() {
        let units = vec![
            completed(WorkUnitRole::OriginRemovalStart, 10),
            pending(WorkUnitRole::QualifiedRelocation, 20),
        ];
        let bundle = derive_bundle(0, 0, CancellationEffectRecipe::QualifiedRelocation, &units)
            .expect("qualified-relocation derivation")
            .expect("marker bundle");
        assert_eq!(
            bundle.outcome(),
            CancellationEffectOutcome::RemovalUnresolved
        );
        assert_eq!(bundle.facts().len(), 1);
        assert_eq!(
            bundle.facts()[0].kind(),
            CancellationEffectFactKind::OriginRemovalStart
        );
    }

    #[test]
    fn invalid_bundle_shape_is_propagated() {
        let units = vec![
            completed(WorkUnitRole::Publish, 20),
            completed(WorkUnitRole::OriginRemovalStart, 10),
        ];
        let error = derive_bundle(0, 0, CancellationEffectRecipe::StagedMove, &units)
            .expect_err("invalid fact order must be reported");
        assert!(matches!(
            error,
            CancellationEffectInventoryError::Shape(
                CancellationEffectInventoryValidationError::FactsNotSortedOrUnique { .. }
            )
        ));
    }

    #[test]
    fn preparation_only_segment_has_no_effect_bundle() {
        let units = vec![completed(WorkUnitRole::Prepare, 20)];
        assert!(
            derive_bundle(0, 0, CancellationEffectRecipe::StagedPublish, &units)
                .expect("preparation-only derivation")
                .is_none()
        );
    }
}
