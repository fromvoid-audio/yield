//! Bounded cancellation-local resource-cleanup observations.
//!
//! This module records observations for the exact applicable staging and retained-recovery
//! obligations in a validated cleanup manifest.  The values do not complete normal progress,
//! prove adoption or absence, discharge operation-wide claims, or grant cleanup authority.

use thiserror::Error;

use super::cancellation_effects::CancellationCleanupManifest;
use super::participants::{ParticipantValidationError, StoredSourceFence};
use super::work_plan::{WorkUnitKey, WorkUnitRole};
use super::work_progress::CompletionContext;

/// Maximum number of applicable resource-cleanup keys retained by one cancellation record.
pub const MAX_CANCELLATION_RESOURCE_CLEANUP_KEYS: usize =
    super::cancellation_effects::MAX_CANCELLATION_CLEANUP_KEYS;
/// Maximum number of resource-cleanup observation rows retained by one record.
pub const MAX_CANCELLATION_RESOURCE_CLEANUP_ROWS: usize = MAX_CANCELLATION_RESOURCE_CLEANUP_KEYS;

/// Exact applicable staging and retained-recovery keys selected from one cleanup manifest.
///
/// The complete cleanup manifest, including operation-wide recovery claim identities, remains
/// bound to this view.  The selected keys are only planned resource obligations; they do not
/// prove that a resource was entered, adopted, preserved, or removed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancellationResourceCleanupManifest {
    cleanup_manifest: CancellationCleanupManifest,
    keys: Vec<WorkUnitKey>,
}

impl CancellationResourceCleanupManifest {
    /// Construct one untrusted resource-cleanup manifest candidate.
    pub fn new(
        cleanup_manifest: CancellationCleanupManifest,
        keys: Vec<WorkUnitKey>,
    ) -> Result<Self, CancellationResourceCleanupManifestValidationError> {
        let manifest = Self {
            cleanup_manifest,
            keys,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    /// Derive the canonical applicable resource keys from a validated cleanup manifest.
    pub fn derive(
        cleanup_manifest: &CancellationCleanupManifest,
    ) -> Result<Self, CancellationResourceCleanupManifestValidationError> {
        let mut keys = cleanup_manifest
            .segments()
            .iter()
            .flat_map(|segment| segment.applicable_keys())
            .filter(|key| {
                matches!(
                    key.role(),
                    WorkUnitRole::Stage | WorkUnitRole::RecoveryMaterialRetained
                )
            })
            .copied()
            .collect::<Vec<_>>();
        keys.sort();
        keys.dedup();
        Self::new(cleanup_manifest.clone(), keys)
    }

    /// Validate the bounded key shape without consulting live progress.
    pub fn validate(&self) -> Result<(), CancellationResourceCleanupManifestValidationError> {
        if self.keys.len() > MAX_CANCELLATION_RESOURCE_CLEANUP_KEYS {
            return Err(
                CancellationResourceCleanupManifestValidationError::TooManyKeys {
                    actual: self.keys.len(),
                    maximum: MAX_CANCELLATION_RESOURCE_CLEANUP_KEYS,
                },
            );
        }
        for (index, key) in self.keys.iter().enumerate().skip(1) {
            if self.keys[index - 1] >= *key {
                return Err(
                    CancellationResourceCleanupManifestValidationError::KeysNotSortedOrUnique {
                        index,
                    },
                );
            }
        }
        if self.keys.iter().any(|key| {
            !matches!(
                key.role(),
                WorkUnitRole::Stage | WorkUnitRole::RecoveryMaterialRetained
            )
        }) {
            return Err(CancellationResourceCleanupManifestValidationError::UnexpectedRole);
        }
        Ok(())
    }

    /// Validate exact cleanup-manifest lineage and canonical resource-key coverage.
    pub fn validate_against(
        &self,
        expected: &CancellationResourceCleanupManifest,
    ) -> Result<(), CancellationResourceCleanupManifestError> {
        self.validate()?;
        expected.validate()?;
        if self.cleanup_manifest != expected.cleanup_manifest {
            return Err(CancellationResourceCleanupManifestError::CleanupManifestMismatch);
        }
        if self.keys != expected.keys {
            return Err(CancellationResourceCleanupManifestError::CandidateMismatch);
        }
        Ok(())
    }

    /// Borrow the complete cleanup manifest, including operation-wide recovery claim IDs.
    pub fn cleanup_manifest(&self) -> &CancellationCleanupManifest {
        &self.cleanup_manifest
    }

    /// Borrow the exact dispatch outcome retained by the bound cleanup lineage.
    pub fn dispatch_outcome(
        &self,
    ) -> &super::cancellation_dispatch::CancellationDispatchOutcomeRecord {
        self.cleanup_manifest.dispatch_outcome()
    }

    /// Borrow canonical applicable staging and retained-recovery keys.
    pub fn keys(&self) -> &[WorkUnitKey] {
        &self.keys
    }
}

/// Failure while validating the bounded resource-cleanup manifest shape.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CancellationResourceCleanupManifestValidationError {
    /// The manifest retained more resource keys than the frozen-plan bound.
    #[error("cancellation resource-cleanup manifest has {actual} keys, maximum is {maximum}")]
    TooManyKeys {
        /// Number of supplied keys.
        actual: usize,
        /// Maximum accepted key count.
        maximum: usize,
    },
    /// Resource keys were not strictly sorted or contain a duplicate.
    #[error(
        "cancellation resource-cleanup manifest keys are not sorted or unique at index {index}"
    )]
    KeysNotSortedOrUnique {
        /// Index at which ordering failed.
        index: usize,
    },
    /// A candidate named a role outside the resource-cleanup contract.
    #[error("cancellation resource-cleanup manifest contains an unexpected work role")]
    UnexpectedRole,
}

/// Failure while validating a resource-cleanup manifest against canonical inputs.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CancellationResourceCleanupManifestError {
    /// The candidate had an invalid local shape.
    #[error("invalid cancellation resource-cleanup manifest shape: {0}")]
    Shape(#[source] CancellationResourceCleanupManifestValidationError),
    /// The candidate retained a different full cleanup manifest.
    #[error("cancellation resource-cleanup manifest cleanup lineage does not match")]
    CleanupManifestMismatch,
    /// The candidate keys did not match canonical applicable resource keys.
    #[error("cancellation resource-cleanup manifest candidate does not match")]
    CandidateMismatch,
}

impl From<CancellationResourceCleanupManifestValidationError>
    for CancellationResourceCleanupManifestError
{
    fn from(error: CancellationResourceCleanupManifestValidationError) -> Self {
        Self::Shape(error)
    }
}

/// One cancellation-local resource observation outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CancellationResourceCleanupOutcome {
    /// No resource observation has been retained for this planned key.
    Pending,
    /// The owner observed the resource preserved, without proving adoption or qualification.
    Preserved {
        /// Positive owner-local observation revision, unrelated to source revisions.
        owner_revision: u64,
        /// Historical participant context accepted with the observation.
        context: CompletionContext,
    },
    /// The owner observed the resource absent, without proving it was never entered or releasable.
    Absent {
        /// Positive owner-local observation revision, unrelated to source revisions.
        owner_revision: u64,
        /// Historical participant context accepted with the observation.
        context: CompletionContext,
    },
}

/// One exact resource-cleanup key and its typed cancellation-local observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancellationResourceCleanupObservationRow {
    key: WorkUnitKey,
    outcome: CancellationResourceCleanupOutcome,
}

impl CancellationResourceCleanupObservationRow {
    /// Construct one untrusted resource-cleanup observation row.
    pub fn new(key: WorkUnitKey, outcome: CancellationResourceCleanupOutcome) -> Self {
        Self { key, outcome }
    }

    /// Return the exact resource key.
    pub const fn key(&self) -> WorkUnitKey {
        self.key
    }

    /// Borrow the typed resource observation.
    pub fn outcome(&self) -> &CancellationResourceCleanupOutcome {
        &self.outcome
    }
}

/// Immutable cancellation-local resource-cleanup observations bound to one exact manifest.
///
/// This record is observational only.  It never completes normal steps, proves adoption or
/// absence, maps or discharges operation-wide claims, grants cleanup authority, or authorizes a
/// terminal transition.  `Preserved` is not adopted, and `Absent` does not prove `NeverEntered`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancellationResourceCleanupObservationRecord {
    manifest: CancellationResourceCleanupManifest,
    observation_epoch: u64,
    observation_generation: u64,
    rows: Vec<CancellationResourceCleanupObservationRow>,
}

/// Failure while validating one resource-cleanup observation record.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum CancellationResourceCleanupObservationRecordError {
    /// The bound resource manifest was malformed.
    #[error("invalid cancellation resource-cleanup manifest: {0}")]
    Manifest(#[source] CancellationResourceCleanupManifestError),
    /// The record carried no positive observation generation.
    #[error("cancellation resource-cleanup observation generation must be greater than zero")]
    ObservationGenerationZero,
    /// The observation boundary predates its bound dispatch observation.
    #[error(
        "cancellation resource-cleanup observation epoch {actual} predates dispatch epoch {dispatch}"
    )]
    ObservationEpochBeforeDispatch {
        /// Epoch carried by the candidate record.
        actual: u64,
        /// Epoch retained by the bound dispatch record.
        dispatch: u64,
    },
    /// The record exceeded the bounded resource-row count.
    #[error("cancellation resource-cleanup observation has {actual} rows, maximum is {maximum}")]
    TooManyRows {
        /// Number of supplied rows.
        actual: usize,
        /// Maximum accepted rows.
        maximum: usize,
    },
    /// Rows were not strictly sorted or contained a duplicate key.
    #[error(
        "cancellation resource-cleanup observation rows are not sorted or unique at index {index}"
    )]
    RowsNotSortedOrUnique {
        /// Index at which ordering failed.
        index: usize,
    },
    /// An owner-local revision was not positive.
    #[error("cancellation resource-cleanup owner revision is zero for {key:?}")]
    OwnerRevisionZero {
        /// Key carrying the invalid revision.
        key: WorkUnitKey,
    },
    /// A nested historical source fence was malformed.
    #[error("invalid cancellation resource-cleanup context for {key:?}: {source}")]
    ContextScope {
        /// Key carrying the malformed context.
        key: WorkUnitKey,
        /// Nested participant validation failure.
        #[source]
        source: ParticipantValidationError,
    },
    /// A late context was newer than the record boundary.
    #[error(
        "cancellation resource-cleanup context epoch {context_epoch} is newer than observation epoch {observation_epoch} for {key:?}"
    )]
    ContextEpochAfterObservation {
        /// Key carrying the late context.
        key: WorkUnitKey,
        /// Context execution epoch.
        context_epoch: u64,
        /// Record observation epoch.
        observation_epoch: u64,
    },
    /// The record did not retain the expected resource manifest.
    #[error("cancellation resource-cleanup observation manifest does not match")]
    ManifestMismatch,
    /// The observation boundary is newer than current progress.
    #[error(
        "cancellation resource-cleanup observation epoch {actual} is newer than current epoch {current}"
    )]
    ObservationEpochFuture {
        /// Epoch carried by the candidate record.
        actual: u64,
        /// Current progress execution epoch.
        current: u64,
    },
    /// The record did not cover every expected resource key exactly once.
    #[error(
        "cancellation resource-cleanup observation row count {actual} does not match manifest count {expected}"
    )]
    RowCountMismatch {
        /// Number of supplied rows.
        actual: usize,
        /// Number of expected keys.
        expected: usize,
    },
    /// A row key did not match the canonical key at its position.
    #[error(
        "cancellation resource-cleanup observation row {index} key {actual:?} does not match manifest key {expected:?}"
    )]
    RowKeyMismatch {
        /// Row position carrying the mismatch.
        index: usize,
        /// Supplied row key.
        actual: WorkUnitKey,
        /// Expected manifest key.
        expected: WorkUnitKey,
    },
}

impl CancellationResourceCleanupObservationRecord {
    /// Construct one untrusted resource-cleanup record after local shape validation.
    pub fn new(
        manifest: CancellationResourceCleanupManifest,
        observation_epoch: u64,
        observation_generation: u64,
        rows: Vec<CancellationResourceCleanupObservationRow>,
    ) -> Result<Self, CancellationResourceCleanupObservationRecordError> {
        let record = Self {
            manifest,
            observation_epoch,
            observation_generation,
            rows,
        };
        record.validate()?;
        Ok(record)
    }

    /// Validate local shape, nested fence shape, and epoch lineage.
    pub fn validate(&self) -> Result<(), CancellationResourceCleanupObservationRecordError> {
        self.manifest
            .validate()
            .map_err(CancellationResourceCleanupManifestError::from)
            .map_err(CancellationResourceCleanupObservationRecordError::Manifest)?;
        if self.observation_generation == 0 {
            return Err(
                CancellationResourceCleanupObservationRecordError::ObservationGenerationZero,
            );
        }
        let dispatch_epoch = self.manifest.dispatch_outcome().observation_epoch();
        if self.observation_epoch < dispatch_epoch {
            return Err(
                CancellationResourceCleanupObservationRecordError::ObservationEpochBeforeDispatch {
                    actual: self.observation_epoch,
                    dispatch: dispatch_epoch,
                },
            );
        }
        if self.rows.len() > MAX_CANCELLATION_RESOURCE_CLEANUP_ROWS {
            return Err(
                CancellationResourceCleanupObservationRecordError::TooManyRows {
                    actual: self.rows.len(),
                    maximum: MAX_CANCELLATION_RESOURCE_CLEANUP_ROWS,
                },
            );
        }
        for (index, row) in self.rows.iter().enumerate() {
            if index > 0 && self.rows[index - 1].key >= row.key {
                return Err(
                    CancellationResourceCleanupObservationRecordError::RowsNotSortedOrUnique {
                        index,
                    },
                );
            }
            let (owner_revision, context) = match row.outcome() {
                CancellationResourceCleanupOutcome::Pending => continue,
                CancellationResourceCleanupOutcome::Preserved {
                    owner_revision,
                    context,
                }
                | CancellationResourceCleanupOutcome::Absent {
                    owner_revision,
                    context,
                } => (*owner_revision, context),
            };
            if owner_revision == 0 {
                return Err(
                    CancellationResourceCleanupObservationRecordError::OwnerRevisionZero {
                        key: row.key,
                    },
                );
            }
            context
                .scope()
                .source_fence()
                .map(StoredSourceFence::validate)
                .transpose()
                .map_err(|source| {
                    CancellationResourceCleanupObservationRecordError::ContextScope {
                        key: row.key,
                        source,
                    }
                })?;
            if context.execution_epoch() > self.observation_epoch {
                return Err(
                    CancellationResourceCleanupObservationRecordError::ContextEpochAfterObservation {
                        key: row.key,
                        context_epoch: context.execution_epoch(),
                        observation_epoch: self.observation_epoch,
                    },
                );
            }
        }
        Ok(())
    }

    /// Validate exact manifest coverage and current observation epoch.
    pub fn validate_against(
        &self,
        manifest: &CancellationResourceCleanupManifest,
        current_execution_epoch: u64,
    ) -> Result<(), CancellationResourceCleanupObservationRecordError> {
        self.validate()?;
        self.manifest
            .validate_against(manifest)
            .map_err(|_| CancellationResourceCleanupObservationRecordError::ManifestMismatch)?;
        if self.observation_epoch > current_execution_epoch {
            return Err(
                CancellationResourceCleanupObservationRecordError::ObservationEpochFuture {
                    actual: self.observation_epoch,
                    current: current_execution_epoch,
                },
            );
        }
        if self.rows.len() != manifest.keys().len() {
            return Err(
                CancellationResourceCleanupObservationRecordError::RowCountMismatch {
                    actual: self.rows.len(),
                    expected: manifest.keys().len(),
                },
            );
        }
        for (index, (expected, row)) in manifest.keys().iter().zip(&self.rows).enumerate() {
            if row.key != *expected {
                return Err(
                    CancellationResourceCleanupObservationRecordError::RowKeyMismatch {
                        index,
                        actual: row.key,
                        expected: *expected,
                    },
                );
            }
        }
        Ok(())
    }

    /// Borrow the exact resource-cleanup manifest bound to this record.
    pub fn manifest(&self) -> &CancellationResourceCleanupManifest {
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

    /// Borrow exact sorted resource-cleanup observation rows.
    pub fn rows(&self) -> &[CancellationResourceCleanupObservationRow] {
        &self.rows
    }
}
