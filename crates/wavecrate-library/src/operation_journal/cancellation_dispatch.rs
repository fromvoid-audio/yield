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
use super::participants::{ParticipantValidationError, StoredSourceFence};
use super::work_plan::{WorkPlan, WorkUnitApplicability, WorkUnitKey, WorkUnitRole};
use super::work_progress::{
    CompletionContext, ProgressEvidenceError, ProgressStepStatus, StoredStepEvidence,
    WorkProgressStep,
};

/// Maximum number of dispatch-obligation keys retained for one frozen cancellation inventory.
pub const MAX_CANCELLATION_DISPATCH_KEYS: usize = super::work_plan::MAX_WORK_PLAN_UNITS;
/// Maximum number of per-key cancellation dispatch outcome rows retained in one observation.
pub const MAX_CANCELLATION_DISPATCH_ROWS: usize = MAX_CANCELLATION_DISPATCH_KEYS;

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

/// One typed observational result for a frozen cancellation dispatch obligation.
///
/// Only `ObservedAfterCancellation` carries late evidence.  The other variants deliberately
/// carry no evidence or context, so a caller cannot represent an accidental late-completion
/// payload under an uncertain or no-entry result.  None of these observations grants dispatch,
/// barrier, recovery, cleanup, or terminal authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CancellationDispatchOutcome {
    /// No dispatch entry was observed for the obligation.
    NeverEntered,
    /// The observation boundary could not establish whether dispatch entry occurred.
    EntryUncertain,
    /// A late completion was observed after cancellation, with its accepted historical context.
    ObservedAfterCancellation {
        /// Non-authoritative evidence retained with the late completion.
        evidence: StoredStepEvidence,
        /// Historical participant scope and execution epoch accepted with the completion.
        context: CompletionContext,
    },
}

/// One exact frozen dispatch key and its cancellation-wide outcome observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancellationDispatchOutcomeRow {
    key: WorkUnitKey,
    outcome: CancellationDispatchOutcome,
}

impl CancellationDispatchOutcomeRow {
    /// Construct one untrusted observational row.
    pub fn new(key: WorkUnitKey, outcome: CancellationDispatchOutcome) -> Self {
        Self { key, outcome }
    }

    /// Return the exact frozen dispatch key.
    pub const fn key(&self) -> WorkUnitKey {
        self.key
    }

    /// Borrow the typed cancellation outcome.
    pub fn outcome(&self) -> &CancellationDispatchOutcome {
        &self.outcome
    }
}

/// One immutable cancellation-wide dispatch outcome observation.
///
/// The record binds a frozen dispatch manifest to the observation epoch and recovery generation
/// that produced its exact rows.  It remains a candidate observation until a future private
/// barrier owner validates and accepts it; constructing or validating this value does not grant
/// authority and does not mutate `WorkProgress`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancellationDispatchOutcomeRecord {
    manifest: CancellationDispatchManifest,
    observation_epoch: u64,
    recovery_generation: u64,
    rows: Vec<CancellationDispatchOutcomeRow>,
}

/// Failure while validating one cancellation dispatch outcome observation or refinement.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum CancellationDispatchOutcomeRecordError {
    /// The record carried a malformed frozen manifest shape.
    #[error("invalid cancellation dispatch outcome manifest: {0}")]
    ManifestShape(#[source] CancellationDispatchManifestValidationError),
    /// The record carried no positive recovery generation.
    #[error("cancellation dispatch outcome recovery generation must be greater than zero")]
    RecoveryGenerationZero,
    /// The observation boundary predates the frozen cancellation capture.
    #[error(
        "cancellation dispatch outcome observation epoch {actual} predates capture epoch {capture}"
    )]
    ObservationEpochBeforeCapture {
        /// Epoch carried by the candidate record.
        actual: u64,
        /// Capture epoch bound to the frozen cancellation manifest.
        capture: u64,
    },
    /// The record exceeded the bounded frozen dispatch row count.
    #[error("cancellation dispatch outcome has {actual} rows, maximum is {maximum}")]
    TooManyRows {
        /// Number of rows supplied by the candidate.
        actual: usize,
        /// Maximum number of rows accepted by the in-memory bound.
        maximum: usize,
    },
    /// Rows were not in exact frozen-key order or contained a duplicate key.
    #[error("cancellation dispatch outcome rows are not sorted or unique at index {index}")]
    RowsNotSortedOrUnique {
        /// Index at which ordering or uniqueness failed.
        index: usize,
    },
    /// A late row's nested source fence was malformed.
    #[error("invalid cancellation dispatch outcome context for {key:?}: {source}")]
    ContextScope {
        /// Key carrying the malformed context.
        key: WorkUnitKey,
        /// Nested participant-scope validation failure.
        #[source]
        source: ParticipantValidationError,
    },
    /// A late row's nested evidence was malformed.
    #[error("invalid cancellation dispatch outcome evidence for {key:?}: {source}")]
    Evidence {
        /// Key carrying the malformed evidence.
        key: WorkUnitKey,
        /// Nested evidence validation failure.
        #[source]
        source: ProgressEvidenceError,
    },
    /// A late completion context was newer than the record's observation boundary.
    #[error(
        "cancellation dispatch outcome context epoch {context_epoch} is newer than observation epoch {observation_epoch} for {key:?}"
    )]
    ContextEpochAfterObservation {
        /// Key carrying the late context.
        key: WorkUnitKey,
        /// Context epoch carried by the row.
        context_epoch: u64,
        /// Observation epoch carried by the record.
        observation_epoch: u64,
    },
    /// The record manifest did not exactly match the expected frozen manifest.
    #[error("cancellation dispatch outcome manifest does not match the expected frozen manifest")]
    ManifestMismatch,
    /// The record observation epoch was newer than current progress.
    #[error(
        "cancellation dispatch outcome observation epoch {actual} is newer than current epoch {current}"
    )]
    ObservationEpochFuture {
        /// Epoch carried by the candidate record.
        actual: u64,
        /// Current progress execution epoch.
        current: u64,
    },
    /// The record did not cover exactly every manifest key.
    #[error(
        "cancellation dispatch outcome row count {actual} does not match manifest count {expected}"
    )]
    RowCountMismatch {
        /// Number of rows supplied by the candidate.
        actual: usize,
        /// Number of frozen dispatch keys required by the manifest.
        expected: usize,
    },
    /// A row key did not match the manifest key at its exact position.
    #[error(
        "cancellation dispatch outcome row {index} key {actual:?} does not match manifest key {expected:?}"
    )]
    RowKeyMismatch {
        /// Row position carrying the mismatch.
        index: usize,
        /// Key supplied by the candidate row.
        actual: WorkUnitKey,
        /// Key required by the frozen manifest.
        expected: WorkUnitKey,
    },
    /// A non-identical refinement did not advance the recovery generation.
    #[error(
        "cancellation dispatch outcome recovery generation {actual} is not newer than {previous}"
    )]
    RecoveryGenerationRegression {
        /// Previously accepted generation.
        previous: u64,
        /// Candidate generation.
        actual: u64,
    },
    /// A refinement moved its observation boundary backwards.
    #[error("cancellation dispatch outcome observation epoch {actual} regressed below {previous}")]
    ObservationEpochRegression {
        /// Previously accepted observation epoch.
        previous: u64,
        /// Candidate observation epoch.
        actual: u64,
    },
    /// A no-entry observation cannot later be changed into another outcome.
    #[error("cancellation dispatch outcome changed NeverEntered for {key:?}")]
    NeverEnteredChanged {
        /// Key whose no-entry result was rewritten.
        key: WorkUnitKey,
    },
    /// An accepted late completion's evidence or context was rewritten.
    #[error("cancellation dispatch outcome late completion changed for {key:?}")]
    LateObservationChanged {
        /// Key whose accepted late observation was rewritten.
        key: WorkUnitKey,
    },
}

impl CancellationDispatchOutcomeRecord {
    /// Construct one candidate observational record after local shape validation.
    pub fn new(
        manifest: CancellationDispatchManifest,
        observation_epoch: u64,
        recovery_generation: u64,
        rows: Vec<CancellationDispatchOutcomeRow>,
    ) -> Result<Self, CancellationDispatchOutcomeRecordError> {
        let record = Self {
            manifest,
            observation_epoch,
            recovery_generation,
            rows,
        };
        record.validate()?;
        Ok(record)
    }

    /// Validate local record shape and nested observational payloads.
    pub fn validate(&self) -> Result<(), CancellationDispatchOutcomeRecordError> {
        self.manifest
            .validate()
            .map_err(CancellationDispatchOutcomeRecordError::ManifestShape)?;
        if self.recovery_generation == 0 {
            return Err(CancellationDispatchOutcomeRecordError::RecoveryGenerationZero);
        }
        if self.observation_epoch < self.manifest.capture_epoch() {
            return Err(
                CancellationDispatchOutcomeRecordError::ObservationEpochBeforeCapture {
                    actual: self.observation_epoch,
                    capture: self.manifest.capture_epoch(),
                },
            );
        }
        if self.rows.len() > MAX_CANCELLATION_DISPATCH_ROWS {
            return Err(CancellationDispatchOutcomeRecordError::TooManyRows {
                actual: self.rows.len(),
                maximum: MAX_CANCELLATION_DISPATCH_ROWS,
            });
        }
        for (index, row) in self.rows.iter().enumerate() {
            if index > 0 && self.rows[index - 1].key() >= row.key() {
                return Err(
                    CancellationDispatchOutcomeRecordError::RowsNotSortedOrUnique { index },
                );
            }
            if let CancellationDispatchOutcome::ObservedAfterCancellation { evidence, context } =
                row.outcome()
            {
                context
                    .scope()
                    .source_fence()
                    .map(StoredSourceFence::validate)
                    .transpose()
                    .map_err(
                        |source| CancellationDispatchOutcomeRecordError::ContextScope {
                            key: row.key(),
                            source,
                        },
                    )?;
                evidence.validate().map_err(|source| {
                    CancellationDispatchOutcomeRecordError::Evidence {
                        key: row.key(),
                        source,
                    }
                })?;
                if context.execution_epoch() > self.observation_epoch {
                    return Err(
                        CancellationDispatchOutcomeRecordError::ContextEpochAfterObservation {
                            key: row.key(),
                            context_epoch: context.execution_epoch(),
                            observation_epoch: self.observation_epoch,
                        },
                    );
                }
            }
        }
        Ok(())
    }

    /// Validate this record against the exact current frozen manifest and epoch.
    pub fn validate_against(
        &self,
        manifest: &CancellationDispatchManifest,
        current_execution_epoch: u64,
    ) -> Result<(), CancellationDispatchOutcomeRecordError> {
        self.validate()?;
        if &self.manifest != manifest {
            return Err(CancellationDispatchOutcomeRecordError::ManifestMismatch);
        }
        if self.observation_epoch > current_execution_epoch {
            return Err(
                CancellationDispatchOutcomeRecordError::ObservationEpochFuture {
                    actual: self.observation_epoch,
                    current: current_execution_epoch,
                },
            );
        }
        if self.rows.len() != manifest.keys().len() {
            return Err(CancellationDispatchOutcomeRecordError::RowCountMismatch {
                actual: self.rows.len(),
                expected: manifest.keys().len(),
            });
        }
        for (index, (expected, row)) in manifest.keys().iter().zip(&self.rows).enumerate() {
            if row.key() != *expected {
                return Err(CancellationDispatchOutcomeRecordError::RowKeyMismatch {
                    index,
                    actual: row.key(),
                    expected: *expected,
                });
            }
        }
        Ok(())
    }

    /// Validate a monotonic refinement against a previously accepted observation.
    ///
    /// Exact replay is idempotent.  Any non-identical candidate must advance the recovery
    /// generation and retain or advance the observation epoch.  An uncertain entry may refine to
    /// any typed outcome; a no-entry result and an observed late completion cannot be rewritten.
    pub fn validate_transition(
        &self,
        previous: &Self,
        current_execution_epoch: u64,
    ) -> Result<(), CancellationDispatchOutcomeRecordError> {
        previous.validate_against(&previous.manifest, current_execution_epoch)?;
        self.validate_against(&previous.manifest, current_execution_epoch)?;
        if self == previous {
            return Ok(());
        }
        if self.recovery_generation <= previous.recovery_generation {
            return Err(
                CancellationDispatchOutcomeRecordError::RecoveryGenerationRegression {
                    previous: previous.recovery_generation,
                    actual: self.recovery_generation,
                },
            );
        }
        if self.observation_epoch < previous.observation_epoch {
            return Err(
                CancellationDispatchOutcomeRecordError::ObservationEpochRegression {
                    previous: previous.observation_epoch,
                    actual: self.observation_epoch,
                },
            );
        }
        for (previous_row, candidate_row) in previous.rows.iter().zip(&self.rows) {
            match (previous_row.outcome(), candidate_row.outcome()) {
                (
                    CancellationDispatchOutcome::NeverEntered,
                    CancellationDispatchOutcome::NeverEntered,
                )
                | (CancellationDispatchOutcome::EntryUncertain, _)
                | (
                    CancellationDispatchOutcome::ObservedAfterCancellation { .. },
                    CancellationDispatchOutcome::ObservedAfterCancellation { .. },
                ) if previous_row.outcome() == candidate_row.outcome() => {}
                (CancellationDispatchOutcome::NeverEntered, _) => {
                    return Err(
                        CancellationDispatchOutcomeRecordError::NeverEnteredChanged {
                            key: previous_row.key(),
                        },
                    );
                }
                (
                    CancellationDispatchOutcome::ObservedAfterCancellation {
                        evidence: previous_evidence,
                        context: previous_context,
                    },
                    CancellationDispatchOutcome::ObservedAfterCancellation {
                        evidence: candidate_evidence,
                        context: candidate_context,
                    },
                ) if previous_evidence == candidate_evidence
                    && previous_context == candidate_context => {}
                (CancellationDispatchOutcome::ObservedAfterCancellation { .. }, _) => {
                    return Err(
                        CancellationDispatchOutcomeRecordError::LateObservationChanged {
                            key: previous_row.key(),
                        },
                    );
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Borrow the frozen manifest bound to this observation.
    pub fn manifest(&self) -> &CancellationDispatchManifest {
        &self.manifest
    }

    /// Return the execution epoch at which this observation was made.
    pub const fn observation_epoch(&self) -> u64 {
        self.observation_epoch
    }

    /// Return the one-based recovery-observation generation.
    pub const fn recovery_generation(&self) -> u64 {
        self.recovery_generation
    }

    /// Borrow exact sorted per-key outcome rows.
    pub fn rows(&self) -> &[CancellationDispatchOutcomeRow] {
        &self.rows
    }
}
