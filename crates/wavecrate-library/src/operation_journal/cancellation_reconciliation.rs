//! Bounded cancellation-local source-closure observations.
//!
//! This module retains only the source-closure obligations exposed by a validated after-effect
//! inventory.  It records observational receipts without completing normal progress, issuing a
//! source capability, or authorizing cleanup, settlement, terminal cancellation, or persistence.

use thiserror::Error;

use super::cancellation_effects::CancellationEffectRecoveryInventory;
use super::participants::{ParticipantValidationError, StoredSourceFence};
use super::work_plan::WorkUnitKey;
use super::work_progress::{CompletionContext, ProgressEvidenceError, StoredStepEvidence};

/// Maximum number of pending source-closure keys retained by one cancellation observation.
pub const MAX_CANCELLATION_SOURCE_CLOSURE_KEYS: usize =
    super::cancellation_effects::MAX_CANCELLATION_EFFECT_ROWS;
/// Maximum number of source-closure observation rows retained by one record.
pub const MAX_CANCELLATION_SOURCE_CLOSURE_ROWS: usize = MAX_CANCELLATION_SOURCE_CLOSURE_KEYS;

/// Exact source-closure keys still pending in one validated after-effect inventory.
///
/// The manifest retains the complete recovery inventory that produced it.  It is an observation
/// of cancellation-local obligations only: membership, pending status, or an absent key does not
/// prove source state, grant a source capability, or authorize recovery or cleanup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceClosureManifest {
    recovery_inventory: CancellationEffectRecoveryInventory,
    keys: Vec<WorkUnitKey>,
}

/// Failure while validating the bounded local source-closure manifest shape.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SourceClosureManifestValidationError {
    /// The manifest contains more keys than the bounded plan-derived limit.
    #[error("cancellation source-closure manifest has {actual} keys, maximum is {maximum}")]
    TooManyKeys {
        /// Number of keys supplied by the candidate.
        actual: usize,
        /// Maximum number of keys accepted by the in-memory boundary.
        maximum: usize,
    },
    /// Manifest keys are not strictly sorted or contain a duplicate.
    #[error("cancellation source-closure manifest keys are not sorted or unique at index {index}")]
    KeysNotSortedOrUnique {
        /// Index at which ordering or uniqueness failed.
        index: usize,
    },
}

/// Failure while validating a source-closure manifest against a canonical recovery inventory.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SourceClosureManifestError {
    /// The candidate manifest had an invalid bounded local shape.
    #[error("invalid cancellation source-closure manifest shape: {0}")]
    Shape(#[source] SourceClosureManifestValidationError),
    /// The candidate did not retain the exact full recovery-inventory lineage.
    #[error("cancellation source-closure manifest recovery inventory does not match")]
    RecoveryInventoryMismatch,
    /// The candidate keys did not equal the canonical pending source-closure union.
    #[error("cancellation source-closure manifest does not match the canonical pending union")]
    CandidateMismatch,
}

impl From<SourceClosureManifestValidationError> for SourceClosureManifestError {
    fn from(error: SourceClosureManifestValidationError) -> Self {
        Self::Shape(error)
    }
}

impl SourceClosureManifest {
    /// Construct one untrusted source-closure manifest candidate after local shape validation.
    pub fn new(
        recovery_inventory: CancellationEffectRecoveryInventory,
        keys: Vec<WorkUnitKey>,
    ) -> Result<Self, SourceClosureManifestValidationError> {
        let manifest = Self {
            recovery_inventory,
            keys,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    /// Derive the canonical pending source-closure union from a validated recovery inventory.
    pub(crate) fn derive(
        recovery_inventory: CancellationEffectRecoveryInventory,
    ) -> Result<Self, SourceClosureManifestValidationError> {
        let mut keys = recovery_inventory
            .bundles()
            .iter()
            .flat_map(|bundle| bundle.source_closure())
            .filter(|closure| closure.status() == super::work_progress::ProgressStepStatus::Pending)
            .map(|closure| closure.key())
            .collect::<Vec<_>>();
        keys.sort();
        keys.dedup();
        Self::new(recovery_inventory, keys)
    }

    /// Validate bounded key ordering without interpreting source authority.
    pub fn validate(&self) -> Result<(), SourceClosureManifestValidationError> {
        if self.keys.len() > MAX_CANCELLATION_SOURCE_CLOSURE_KEYS {
            return Err(SourceClosureManifestValidationError::TooManyKeys {
                actual: self.keys.len(),
                maximum: MAX_CANCELLATION_SOURCE_CLOSURE_KEYS,
            });
        }
        for (index, key) in self.keys.iter().enumerate().skip(1) {
            if self.keys[index - 1] >= *key {
                return Err(SourceClosureManifestValidationError::KeysNotSortedOrUnique { index });
            }
        }
        Ok(())
    }

    /// Validate exact recovery lineage and canonical pending-key coverage.
    pub fn validate_against(
        &self,
        expected: &SourceClosureManifest,
    ) -> Result<(), SourceClosureManifestError> {
        self.validate()?;
        expected.validate()?;
        if self.recovery_inventory != expected.recovery_inventory {
            return Err(SourceClosureManifestError::RecoveryInventoryMismatch);
        }
        if self.keys != expected.keys {
            return Err(SourceClosureManifestError::CandidateMismatch);
        }
        Ok(())
    }

    /// Borrow the complete recovery inventory retained with this manifest.
    pub fn recovery_inventory(&self) -> &CancellationEffectRecoveryInventory {
        &self.recovery_inventory
    }

    /// Borrow exact pending source-closure keys in canonical work-unit order.
    pub fn keys(&self) -> &[WorkUnitKey] {
        &self.keys
    }
}

/// One typed cancellation-local source-closure observation.
///
/// `Pending` carries no receipt.  `Observed` retains historical evidence and context without
/// changing the corresponding normal progress step.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CancellationSourceClosureOutcome {
    /// No cancellation-local source receipt has been retained for this closure key.
    Pending,
    /// A cancellation-local source receipt was observed with its accepted historical context.
    Observed {
        /// Non-authoritative evidence retained for the closure observation.
        evidence: StoredStepEvidence,
        /// Historical participant scope and execution epoch accepted with the evidence.
        context: CompletionContext,
    },
}

/// One exact source-closure key and its cancellation-local observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancellationSourceClosureObservationRow {
    key: WorkUnitKey,
    outcome: CancellationSourceClosureOutcome,
}

impl CancellationSourceClosureObservationRow {
    /// Construct one untrusted cancellation-local observation row.
    pub fn new(key: WorkUnitKey, outcome: CancellationSourceClosureOutcome) -> Self {
        Self { key, outcome }
    }

    /// Return the exact source-closure key.
    pub const fn key(&self) -> WorkUnitKey {
        self.key
    }

    /// Borrow the typed cancellation-local outcome.
    pub fn outcome(&self) -> &CancellationSourceClosureOutcome {
        &self.outcome
    }
}

/// Immutable cancellation-local source-closure observations bound to one exact manifest.
///
/// This record is observational data only.  It does not transition normal steps, prove source
/// state, issue a source capability, or authorize cleanup, settlement, terminal cancellation, or
/// persistence.  Transition and refinement policy belongs to a later accepting guard.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancellationSourceClosureObservationRecord {
    manifest: SourceClosureManifest,
    observation_epoch: u64,
    observation_generation: u64,
    rows: Vec<CancellationSourceClosureObservationRow>,
}

/// Failure while validating one cancellation-local source-closure record.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum CancellationSourceClosureObservationRecordError {
    /// The bound manifest had an invalid local shape.
    #[error("invalid cancellation source-closure manifest: {0}")]
    Manifest(#[source] SourceClosureManifestError),
    /// The record carried no positive observation generation.
    #[error("cancellation source-closure observation generation must be greater than zero")]
    ObservationGenerationZero,
    /// The observation boundary predates the bound dispatch observation.
    #[error(
        "cancellation source-closure observation epoch {actual} predates dispatch epoch {dispatch}"
    )]
    ObservationEpochBeforeDispatch {
        /// Epoch carried by the candidate record.
        actual: u64,
        /// Observation epoch retained by the bound dispatch record.
        dispatch: u64,
    },
    /// The record exceeded the bounded source-closure row count.
    #[error("cancellation source-closure observation has {actual} rows, maximum is {maximum}")]
    TooManyRows {
        /// Number of rows supplied by the candidate.
        actual: usize,
        /// Maximum number of rows accepted by the in-memory boundary.
        maximum: usize,
    },
    /// Rows are not strictly sorted or contain a duplicate key.
    #[error(
        "cancellation source-closure observation rows are not sorted or unique at index {index}"
    )]
    RowsNotSortedOrUnique {
        /// Index at which ordering or uniqueness failed.
        index: usize,
    },
    /// A nested source fence was malformed.
    #[error("invalid cancellation source-closure context for {key:?}: {source}")]
    ContextScope {
        /// Key carrying the malformed context.
        key: WorkUnitKey,
        /// Nested participant validation failure.
        #[source]
        source: ParticipantValidationError,
    },
    /// Nested evidence was malformed.
    #[error("invalid cancellation source-closure evidence for {key:?}: {source}")]
    Evidence {
        /// Key carrying malformed evidence.
        key: WorkUnitKey,
        /// Nested progress-evidence validation failure.
        #[source]
        source: ProgressEvidenceError,
    },
    /// A late context is newer than the observation boundary.
    #[error(
        "cancellation source-closure context epoch {context_epoch} is newer than observation epoch {observation_epoch} for {key:?}"
    )]
    ContextEpochAfterObservation {
        /// Key carrying the late context.
        key: WorkUnitKey,
        /// Context epoch carried by the row.
        context_epoch: u64,
        /// Observation epoch carried by the record.
        observation_epoch: u64,
    },
    /// The record manifest did not match the expected canonical manifest.
    #[error("cancellation source-closure observation manifest does not match")]
    ManifestMismatch,
    /// The observation boundary is newer than current progress.
    #[error(
        "cancellation source-closure observation epoch {actual} is newer than current epoch {current}"
    )]
    ObservationEpochFuture {
        /// Epoch carried by the candidate record.
        actual: u64,
        /// Current progress execution epoch.
        current: u64,
    },
    /// The record does not cover exactly every pending source-closure key.
    #[error(
        "cancellation source-closure observation row count {actual} does not match manifest count {expected}"
    )]
    RowCountMismatch {
        /// Number of rows supplied by the candidate.
        actual: usize,
        /// Number of manifest keys required.
        expected: usize,
    },
    /// A row key does not match the manifest key at its exact position.
    #[error(
        "cancellation source-closure observation row {index} key {actual:?} does not match manifest key {expected:?}"
    )]
    RowKeyMismatch {
        /// Row position carrying the mismatch.
        index: usize,
        /// Key supplied by the candidate row.
        actual: WorkUnitKey,
        /// Key required by the manifest.
        expected: WorkUnitKey,
    },
}

impl CancellationSourceClosureObservationRecord {
    /// Construct one untrusted source-closure record after local shape validation.
    pub fn new(
        manifest: SourceClosureManifest,
        observation_epoch: u64,
        observation_generation: u64,
        rows: Vec<CancellationSourceClosureObservationRow>,
    ) -> Result<Self, CancellationSourceClosureObservationRecordError> {
        let record = Self {
            manifest,
            observation_epoch,
            observation_generation,
            rows,
        };
        record.validate()?;
        Ok(record)
    }

    /// Validate local shape and nested evidence without consulting current progress.
    pub fn validate(&self) -> Result<(), CancellationSourceClosureObservationRecordError> {
        self.manifest
            .validate()
            .map_err(|error| SourceClosureManifestError::from(error))
            .map_err(CancellationSourceClosureObservationRecordError::Manifest)?;
        if self.observation_generation == 0 {
            return Err(CancellationSourceClosureObservationRecordError::ObservationGenerationZero);
        }
        let dispatch_epoch = self
            .manifest
            .recovery_inventory()
            .dispatch_outcome()
            .observation_epoch();
        if self.observation_epoch < dispatch_epoch {
            return Err(
                CancellationSourceClosureObservationRecordError::ObservationEpochBeforeDispatch {
                    actual: self.observation_epoch,
                    dispatch: dispatch_epoch,
                },
            );
        }
        if self.rows.len() > MAX_CANCELLATION_SOURCE_CLOSURE_ROWS {
            return Err(
                CancellationSourceClosureObservationRecordError::TooManyRows {
                    actual: self.rows.len(),
                    maximum: MAX_CANCELLATION_SOURCE_CLOSURE_ROWS,
                },
            );
        }
        for (index, row) in self.rows.iter().enumerate() {
            if index > 0 && self.rows[index - 1].key >= row.key {
                return Err(
                    CancellationSourceClosureObservationRecordError::RowsNotSortedOrUnique {
                        index,
                    },
                );
            }
            let CancellationSourceClosureOutcome::Observed { evidence, context } = row.outcome()
            else {
                continue;
            };
            context
                .scope()
                .source_fence()
                .map(StoredSourceFence::validate)
                .transpose()
                .map_err(|source| {
                    CancellationSourceClosureObservationRecordError::ContextScope {
                        key: row.key,
                        source,
                    }
                })?;
            evidence.validate().map_err(|source| {
                CancellationSourceClosureObservationRecordError::Evidence {
                    key: row.key,
                    source,
                }
            })?;
            if context.execution_epoch() > self.observation_epoch {
                return Err(
                    CancellationSourceClosureObservationRecordError::ContextEpochAfterObservation {
                        key: row.key,
                        context_epoch: context.execution_epoch(),
                        observation_epoch: self.observation_epoch,
                    },
                );
            }
        }
        Ok(())
    }

    /// Validate exact manifest coverage and the current observation epoch.
    pub fn validate_against(
        &self,
        manifest: &SourceClosureManifest,
        current_execution_epoch: u64,
    ) -> Result<(), CancellationSourceClosureObservationRecordError> {
        self.validate()?;
        self.manifest
            .validate_against(manifest)
            .map_err(|_| CancellationSourceClosureObservationRecordError::ManifestMismatch)?;
        if self.observation_epoch > current_execution_epoch {
            return Err(
                CancellationSourceClosureObservationRecordError::ObservationEpochFuture {
                    actual: self.observation_epoch,
                    current: current_execution_epoch,
                },
            );
        }
        if self.rows.len() != manifest.keys().len() {
            return Err(
                CancellationSourceClosureObservationRecordError::RowCountMismatch {
                    actual: self.rows.len(),
                    expected: manifest.keys().len(),
                },
            );
        }
        for (index, (expected, row)) in manifest.keys().iter().zip(&self.rows).enumerate() {
            if row.key != *expected {
                return Err(
                    CancellationSourceClosureObservationRecordError::RowKeyMismatch {
                        index,
                        actual: row.key,
                        expected: *expected,
                    },
                );
            }
        }
        Ok(())
    }

    /// Borrow the exact source-closure manifest bound to this record.
    pub fn manifest(&self) -> &SourceClosureManifest {
        &self.manifest
    }

    /// Return the observation execution epoch.
    pub const fn observation_epoch(&self) -> u64 {
        self.observation_epoch
    }

    /// Return the positive observation generation.
    pub const fn observation_generation(&self) -> u64 {
        self.observation_generation
    }

    /// Borrow exact sorted source-closure observation rows.
    pub fn rows(&self) -> &[CancellationSourceClosureObservationRow] {
        &self.rows
    }
}
