//! Bounded cancellation-wide dispatch obligations from a frozen progress inventory.
//!
//! This module records the exact applicable pending work that a future cancellation barrier may
//! need to account for across the complete frozen plan, including later history segments and
//! global recipe units.  It is an immutable observation only: membership, a pending status,
//! idleness, or an absent key proves nothing about whether work entered, whether a barrier is
//! required, or whether any recovery or dispatch authority exists.  The module does not issue a
//! barrier, inspect a source or filesystem, mutate progress, or produce a cancellation outcome.

use thiserror::Error;

use super::admission_claims::AdmissionClaimInventory;
use super::cancellation_recovery::{CancellationInventory, CancellationInventoryError};
use super::model::OperationId;
use super::work_plan::{WorkPlan, WorkUnitApplicability, WorkUnitKey, WorkUnitRole};
use super::work_progress::{ProgressStepStatus, WorkProgressStep};

/// Maximum number of dispatch-obligation keys retained for one frozen cancellation inventory.
pub const MAX_CANCELLATION_DISPATCH_KEYS: usize = super::work_plan::MAX_WORK_PLAN_UNITS;

/// Immutable exact pending dispatch obligations derived from one frozen cancellation inventory.
///
/// The keys remain a conservative plan membership observation.  They do not identify entered
/// work, prove idleness or absence, or grant a barrier, recovery, filesystem, source, or terminal
/// authority.  A retained capture epoch may be older than current progress after source
/// reacquisition; a future issuer can bind a separate barrier to the current epoch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancellationDispatchManifest {
    operation_id: OperationId,
    plan_version: u16,
    frozen_inventory_generation: u64,
    capture_epoch: u64,
    keys: Vec<WorkUnitKey>,
}

/// Failure while validating the bounded dispatch-manifest shape.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CancellationDispatchManifestValidationError {
    /// The manifest contains more keys than the frozen plan bound permits.
    #[error("cancellation dispatch manifest has {actual} keys, maximum is {maximum}")]
    TooManyKeys {
        /// Number of keys supplied by the candidate.
        actual: usize,
        /// Maximum number of keys accepted for one manifest.
        maximum: usize,
    },
    /// Manifest keys are not in exact work-plan order or contain a duplicate.
    #[error("cancellation dispatch manifest keys are not sorted or unique at index {index}")]
    KeysNotSortedOrUnique {
        /// Index at which ordering or uniqueness failed.
        index: usize,
    },
}

/// Failure while deriving or binding a cancellation-wide dispatch manifest.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum CancellationDispatchManifestError {
    /// No frozen cancellation inventory was retained by progress.
    #[error(
        "cancellation dispatch manifest is unavailable without a frozen cancellation inventory"
    )]
    CancellationInventoryMissing,
    /// No exact admission-claim observation was retained by progress.
    #[error("cancellation dispatch manifest requires retained admission claims")]
    AdmissionClaimsMissing,
    /// The frozen cancellation inventory did not validate against current progress.
    #[error("invalid cancellation dispatch manifest source: {0}")]
    Inventory(#[source] CancellationInventoryError),
    /// The plan is not the supported frozen V1 plan.
    #[error("unsupported cancellation dispatch plan version {actual}")]
    UnsupportedPlanVersion {
        /// Version found in the supplied plan.
        actual: u16,
    },
    /// Candidate operation identity did not match the frozen source inventory.
    #[error("cancellation dispatch manifest operation identity does not match")]
    OperationIdMismatch,
    /// Candidate plan version did not match the frozen plan.
    #[error("cancellation dispatch manifest plan version does not match")]
    PlanVersionMismatch,
    /// Candidate frozen-inventory generation did not match the source inventory.
    #[error("cancellation dispatch manifest generation does not match")]
    FrozenInventoryGenerationMismatch,
    /// Candidate capture epoch did not match the source inventory.
    #[error("cancellation dispatch manifest capture epoch does not match")]
    CaptureEpochMismatch,
    /// Candidate local shape was invalid.
    #[error("invalid cancellation dispatch manifest shape: {0}")]
    Shape(#[source] CancellationDispatchManifestValidationError),
    /// Candidate keys did not exactly match the frozen pending derivation.
    #[error("cancellation dispatch manifest candidate does not match the frozen derivation")]
    CandidateMismatch,
}

impl From<CancellationInventoryError> for CancellationDispatchManifestError {
    fn from(error: CancellationInventoryError) -> Self {
        Self::Inventory(error)
    }
}

impl From<CancellationDispatchManifestValidationError> for CancellationDispatchManifestError {
    fn from(error: CancellationDispatchManifestValidationError) -> Self {
        Self::Shape(error)
    }
}

impl CancellationDispatchManifest {
    /// Construct one bounded manifest internally after checking key order and bounds.
    pub(crate) fn new(
        operation_id: OperationId,
        plan_version: u16,
        frozen_inventory_generation: u64,
        capture_epoch: u64,
        keys: Vec<WorkUnitKey>,
    ) -> Result<Self, CancellationDispatchManifestValidationError> {
        let manifest = Self {
            operation_id,
            plan_version,
            frozen_inventory_generation,
            capture_epoch,
            keys,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    /// Validate bounded key ordering without consulting owner authority.
    pub fn validate(&self) -> Result<(), CancellationDispatchManifestValidationError> {
        if self.keys.len() > MAX_CANCELLATION_DISPATCH_KEYS {
            return Err(CancellationDispatchManifestValidationError::TooManyKeys {
                actual: self.keys.len(),
                maximum: MAX_CANCELLATION_DISPATCH_KEYS,
            });
        }
        for (index, key) in self.keys.iter().enumerate().skip(1) {
            if self.keys[index - 1] >= *key {
                return Err(
                    CancellationDispatchManifestValidationError::KeysNotSortedOrUnique { index },
                );
            }
        }
        Ok(())
    }

    /// Validate a candidate against the exact frozen plan, inventory, claims, and current steps.
    ///
    /// Inventory validation deliberately permits a retained capture epoch after source
    /// reacquisition while still requiring exact frozen statuses and claim observations.
    pub fn validate_against(
        &self,
        operation_id: OperationId,
        plan: &WorkPlan,
        inventory: &CancellationInventory,
        current_steps: &[WorkProgressStep],
        admission_claims: &AdmissionClaimInventory,
        execution_epoch: u64,
    ) -> Result<(), CancellationDispatchManifestError> {
        self.validate()?;
        if self.operation_id != operation_id {
            return Err(CancellationDispatchManifestError::OperationIdMismatch);
        }
        if self.plan_version != plan.plan_version() {
            return Err(CancellationDispatchManifestError::PlanVersionMismatch);
        }
        if self.frozen_inventory_generation != inventory.generation() {
            return Err(CancellationDispatchManifestError::FrozenInventoryGenerationMismatch);
        }
        if self.capture_epoch != inventory.execution_epoch() {
            return Err(CancellationDispatchManifestError::CaptureEpochMismatch);
        }
        let expected = derive_cancellation_dispatch_manifest(
            operation_id,
            plan,
            inventory,
            current_steps,
            admission_claims,
            execution_epoch,
        )?;
        if self != &expected {
            return Err(CancellationDispatchManifestError::CandidateMismatch);
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

    /// Return the cancellation-inventory generation captured by this manifest.
    pub const fn frozen_inventory_generation(&self) -> u64 {
        self.frozen_inventory_generation
    }

    /// Return the execution epoch captured with the frozen inventory.
    pub const fn capture_epoch(&self) -> u64 {
        self.capture_epoch
    }

    /// Borrow exact applicable pending non-source-read keys in work-plan order.
    pub fn keys(&self) -> &[WorkUnitKey] {
        &self.keys
    }
}

/// Derive an exact cancellation-wide dispatch manifest after validating the frozen source.
pub(crate) fn derive_cancellation_dispatch_manifest(
    operation_id: OperationId,
    plan: &WorkPlan,
    inventory: &CancellationInventory,
    current_steps: &[WorkProgressStep],
    admission_claims: &AdmissionClaimInventory,
    execution_epoch: u64,
) -> Result<CancellationDispatchManifest, CancellationDispatchManifestError> {
    inventory.validate_against(
        operation_id,
        plan,
        current_steps,
        execution_epoch,
        admission_claims,
    )?;
    if plan.plan_version() != super::work_plan::WORK_PLAN_VERSION_V1 {
        return Err(CancellationDispatchManifestError::UnsupportedPlanVersion {
            actual: plan.plan_version(),
        });
    }

    let mut keys = Vec::new();
    for (unit, snapshot) in plan.units().iter().zip(inventory.steps()) {
        if snapshot.status() == ProgressStepStatus::Pending
            && unit.applicability() == WorkUnitApplicability::Required
            && unit.key().role() != WorkUnitRole::SourceRead
        {
            keys.push(unit.key());
        }
    }
    CancellationDispatchManifest::new(
        operation_id,
        plan.plan_version(),
        inventory.generation(),
        inventory.execution_epoch(),
        keys,
    )
    .map_err(CancellationDispatchManifestError::Shape)
}
