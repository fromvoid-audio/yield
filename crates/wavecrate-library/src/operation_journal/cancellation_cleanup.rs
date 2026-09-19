//! Bounded cancellation-local resource-cleanup observations.
//!
//! This module records observations for the exact applicable staging and retained-recovery
//! obligations in a validated cleanup manifest.  The values do not complete normal progress,
//! prove adoption or absence, discharge operation-wide claims, or grant cleanup authority.
//!
//! An `owner_revision` is a caller-asserted, immutable monotonic observation revision scoped to
//! `(OperationId, WorkUnitKey)` and maintained across execution reacquisition.  Positive shape
//! validation does not authenticate an owner or capability, establish freshness, prove adoption,
//! or grant release authority.

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
        /// Caller-asserted positive revision, monotonic for `(OperationId, WorkUnitKey)` across
        /// reacquisition; validation does not authenticate ownership or prove freshness.
        owner_revision: u64,
        /// Historical participant context accepted with the observation.
        context: CompletionContext,
    },
    /// The owner observed the resource absent, without proving it was never entered or releasable.
    Absent {
        /// Caller-asserted positive revision, monotonic for `(OperationId, WorkUnitKey)` across
        /// reacquisition; validation does not authenticate ownership or prove freshness.
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

/// Maximum number of applicable namespace/disposition keys retained by one cancellation record.
pub const MAX_CANCELLATION_NAMESPACE_CLEANUP_KEYS: usize =
    super::cancellation_effects::MAX_CANCELLATION_CLEANUP_KEYS;
/// Maximum number of namespace/disposition observation rows retained by one record.
pub const MAX_CANCELLATION_NAMESPACE_CLEANUP_ROWS: usize = MAX_CANCELLATION_NAMESPACE_CLEANUP_KEYS;

fn is_namespace_cleanup_role(role: WorkUnitRole) -> bool {
    matches!(
        role,
        WorkUnitRole::Publish
            | WorkUnitRole::QualifiedRelocation
            | WorkUnitRole::RetainedMerge
            | WorkUnitRole::OriginAbsence
            | WorkUnitRole::RetainedObjectAbsence
            | WorkUnitRole::RestoreRecordPrepared
    )
}

fn allows_no_operation_claim(role: WorkUnitRole) -> bool {
    matches!(
        role,
        WorkUnitRole::Publish | WorkUnitRole::QualifiedRelocation | WorkUnitRole::RetainedMerge
    )
}

fn allows_absent(role: WorkUnitRole) -> bool {
    matches!(
        role,
        WorkUnitRole::OriginAbsence
            | WorkUnitRole::RetainedObjectAbsence
            | WorkUnitRole::RestoreRecordPrepared
    )
}

/// Exact applicable namespace/disposition keys selected from one cleanup manifest.
///
/// The complete cleanup manifest remains bound, including its recovery inventory, dispatch
/// outcome, and operation-wide claim identities.  These keys describe local namespace or
/// disposition obligations only; they do not prove entry, absence, adoption, rollback, or
/// completion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancellationNamespaceCleanupManifest {
    cleanup_manifest: CancellationCleanupManifest,
    keys: Vec<WorkUnitKey>,
}

impl CancellationNamespaceCleanupManifest {
    /// Construct one untrusted namespace/disposition manifest candidate.
    pub fn new(
        cleanup_manifest: CancellationCleanupManifest,
        keys: Vec<WorkUnitKey>,
    ) -> Result<Self, CancellationNamespaceCleanupManifestValidationError> {
        let manifest = Self {
            cleanup_manifest,
            keys,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    /// Derive canonical applicable namespace/disposition keys from one cleanup manifest.
    pub fn derive(
        cleanup_manifest: &CancellationCleanupManifest,
    ) -> Result<Self, CancellationNamespaceCleanupManifestValidationError> {
        let mut keys = cleanup_manifest
            .segments()
            .iter()
            .flat_map(|segment| segment.applicable_keys())
            .filter(|key| is_namespace_cleanup_role(key.role()))
            .copied()
            .collect::<Vec<_>>();
        keys.sort();
        keys.dedup();
        Self::new(cleanup_manifest.clone(), keys)
    }

    /// Validate bounded key ordering and role membership without consulting live progress.
    pub fn validate(&self) -> Result<(), CancellationNamespaceCleanupManifestValidationError> {
        if self.keys.len() > MAX_CANCELLATION_NAMESPACE_CLEANUP_KEYS {
            return Err(
                CancellationNamespaceCleanupManifestValidationError::TooManyKeys {
                    actual: self.keys.len(),
                    maximum: MAX_CANCELLATION_NAMESPACE_CLEANUP_KEYS,
                },
            );
        }
        for (index, key) in self.keys.iter().enumerate().skip(1) {
            if self.keys[index - 1] >= *key {
                return Err(
                    CancellationNamespaceCleanupManifestValidationError::KeysNotSortedOrUnique {
                        index,
                    },
                );
            }
        }
        if let Some(key) = self
            .keys
            .iter()
            .copied()
            .find(|key| !is_namespace_cleanup_role(key.role()))
        {
            return Err(
                CancellationNamespaceCleanupManifestValidationError::UnexpectedRole { key },
            );
        }
        Ok(())
    }

    /// Validate exact cleanup-manifest lineage and canonical key coverage.
    pub fn validate_against(
        &self,
        expected: &CancellationNamespaceCleanupManifest,
    ) -> Result<(), CancellationNamespaceCleanupManifestError> {
        self.validate()?;
        expected.validate()?;
        if self.cleanup_manifest != expected.cleanup_manifest {
            return Err(CancellationNamespaceCleanupManifestError::CleanupManifestMismatch);
        }
        if self.keys != expected.keys {
            return Err(CancellationNamespaceCleanupManifestError::CandidateMismatch);
        }
        Ok(())
    }

    /// Borrow the complete cleanup manifest, including operation-wide recovery claim identities.
    pub fn cleanup_manifest(&self) -> &CancellationCleanupManifest {
        &self.cleanup_manifest
    }

    /// Borrow the exact dispatch outcome retained by the bound cleanup lineage.
    pub fn dispatch_outcome(
        &self,
    ) -> &super::cancellation_dispatch::CancellationDispatchOutcomeRecord {
        self.cleanup_manifest.dispatch_outcome()
    }

    /// Borrow canonical applicable namespace/disposition keys.
    pub fn keys(&self) -> &[WorkUnitKey] {
        &self.keys
    }
}

/// Failure while validating the bounded namespace/disposition manifest shape.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CancellationNamespaceCleanupManifestValidationError {
    /// The manifest retained more keys than the frozen-plan bound.
    #[error("cancellation namespace-cleanup manifest has {actual} keys, maximum is {maximum}")]
    TooManyKeys {
        /// Number of supplied keys.
        actual: usize,
        /// Maximum accepted key count.
        maximum: usize,
    },
    /// Namespace/disposition keys were not strictly sorted or contain a duplicate.
    #[error(
        "cancellation namespace-cleanup manifest keys are not sorted or unique at index {index}"
    )]
    KeysNotSortedOrUnique {
        /// Index at which ordering failed.
        index: usize,
    },
    /// A candidate named a role outside the namespace/disposition contract.
    #[error("cancellation namespace-cleanup manifest contains unexpected role for {key:?}")]
    UnexpectedRole {
        /// Key carrying the unexpected role.
        key: WorkUnitKey,
    },
}

/// Failure while validating a namespace/disposition manifest against canonical inputs.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CancellationNamespaceCleanupManifestError {
    /// The candidate had an invalid local shape.
    #[error("invalid cancellation namespace-cleanup manifest shape: {0}")]
    Shape(#[source] CancellationNamespaceCleanupManifestValidationError),
    /// The candidate retained a different full cleanup manifest.
    #[error("cancellation namespace-cleanup manifest cleanup lineage does not match")]
    CleanupManifestMismatch,
    /// The candidate keys did not match canonical applicable keys.
    #[error("cancellation namespace-cleanup manifest candidate does not match")]
    CandidateMismatch,
}

impl From<CancellationNamespaceCleanupManifestValidationError>
    for CancellationNamespaceCleanupManifestError
{
    fn from(error: CancellationNamespaceCleanupManifestValidationError) -> Self {
        Self::Shape(error)
    }
}

/// One typed cancellation-local namespace/disposition observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CancellationNamespaceCleanupOutcome {
    /// No namespace/disposition observation has been retained for this planned key.
    Pending,
    /// The owner observed that no application operation claim was retained.
    ///
    /// This is an untrusted ownership observation.  It is not a NeverEntered proof, rollback
    /// proof, or completion proof.
    NoOperationClaim {
        /// Caller-asserted positive revision, monotonic for `(OperationId, WorkUnitKey)` across
        /// reacquisition; validation does not authenticate ownership or prove freshness.
        owner_revision: u64,
        /// Historical participant scope and execution epoch accepted with the observation.
        context: CompletionContext,
    },
    /// The owner observed the namespace or disposition preserved.
    ///
    /// This does not prove adoption, qualification, or a current capability.
    Preserved {
        /// Caller-asserted positive revision, monotonic for `(OperationId, WorkUnitKey)` across
        /// reacquisition; validation does not authenticate ownership or prove freshness.
        owner_revision: u64,
        /// Historical participant scope and execution epoch accepted with the observation.
        context: CompletionContext,
    },
    /// The owner observed the namespace or disposition absent.
    ///
    /// This does not prove NeverEntered, rollback completion, or release authority.
    Absent {
        /// Caller-asserted positive revision, monotonic for `(OperationId, WorkUnitKey)` across
        /// reacquisition; validation does not authenticate ownership or prove freshness.
        owner_revision: u64,
        /// Historical participant scope and execution epoch accepted with the observation.
        context: CompletionContext,
    },
}

/// One exact namespace/disposition key and its cancellation-local observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancellationNamespaceCleanupObservationRow {
    key: WorkUnitKey,
    outcome: CancellationNamespaceCleanupOutcome,
}

impl CancellationNamespaceCleanupObservationRow {
    /// Construct one untrusted namespace/disposition observation row.
    pub fn new(key: WorkUnitKey, outcome: CancellationNamespaceCleanupOutcome) -> Self {
        Self { key, outcome }
    }

    /// Return the exact namespace/disposition key.
    pub const fn key(&self) -> WorkUnitKey {
        self.key
    }

    /// Borrow the typed cancellation-local outcome.
    pub fn outcome(&self) -> &CancellationNamespaceCleanupOutcome {
        &self.outcome
    }
}

/// Immutable cancellation-local namespace/disposition observations bound to one exact manifest.
///
/// This record is observational only.  It never completes normal progress, proves absence,
/// adoption, rollback, or qualification, maps or discharges operation-wide claims, grants
/// cleanup authority, or authorizes a terminal transition.  Later contradictory observations are
/// retained as observations; a future qualified-outcome guard adjudicates them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancellationNamespaceCleanupObservationRecord {
    manifest: CancellationNamespaceCleanupManifest,
    observation_epoch: u64,
    observation_generation: u64,
    rows: Vec<CancellationNamespaceCleanupObservationRow>,
}

/// Failure while validating one namespace/disposition observation record.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum CancellationNamespaceCleanupObservationRecordError {
    /// The bound namespace/disposition manifest had an invalid shape.
    #[error("invalid cancellation namespace-cleanup manifest: {0}")]
    Manifest(#[source] CancellationNamespaceCleanupManifestError),
    /// The record carried no positive observation generation.
    #[error("cancellation namespace-cleanup observation generation must be greater than zero")]
    ObservationGenerationZero,
    /// The observation boundary predates its bound dispatch observation.
    #[error(
        "cancellation namespace-cleanup observation epoch {actual} predates dispatch epoch {dispatch}"
    )]
    ObservationEpochBeforeDispatch {
        /// Epoch carried by the candidate record.
        actual: u64,
        /// Epoch retained by the bound dispatch record.
        dispatch: u64,
    },
    /// The record exceeded the bounded row count.
    #[error("cancellation namespace-cleanup observation has {actual} rows, maximum is {maximum}")]
    TooManyRows {
        /// Number of supplied rows.
        actual: usize,
        /// Maximum accepted row count.
        maximum: usize,
    },
    /// Rows were not strictly sorted or contained a duplicate key.
    #[error(
        "cancellation namespace-cleanup observation rows are not sorted or unique at index {index}"
    )]
    RowsNotSortedOrUnique {
        /// Index at which ordering failed.
        index: usize,
    },
    /// An owner-local revision was not positive.
    #[error("cancellation namespace-cleanup owner revision is zero for {key:?}")]
    OwnerRevisionZero {
        /// Key carrying the invalid revision.
        key: WorkUnitKey,
    },
    /// No-operation-claim is not admissible for this role.
    #[error(
        "cancellation namespace-cleanup no-operation-claim is not admissible for {key:?} ({role:?})"
    )]
    NoOperationClaimNotAllowed {
        /// Key carrying the disallowed outcome.
        key: WorkUnitKey,
        /// Role selected by the frozen plan.
        role: WorkUnitRole,
    },
    /// Absent is not admissible for this role.
    #[error("cancellation namespace-cleanup absent is not admissible for {key:?} ({role:?})")]
    AbsentNotAllowed {
        /// Key carrying the disallowed outcome.
        key: WorkUnitKey,
        /// Role selected by the frozen plan.
        role: WorkUnitRole,
    },
    /// A nested historical source fence was malformed.
    #[error("invalid cancellation namespace-cleanup context for {key:?}: {source}")]
    ContextScope {
        /// Key carrying the malformed context.
        key: WorkUnitKey,
        /// Nested participant validation failure.
        #[source]
        source: ParticipantValidationError,
    },
    /// A late context was newer than the record boundary.
    #[error(
        "cancellation namespace-cleanup context epoch {context_epoch} is newer than observation epoch {observation_epoch} for {key:?}"
    )]
    ContextEpochAfterObservation {
        /// Key carrying the late context.
        key: WorkUnitKey,
        /// Context execution epoch.
        context_epoch: u64,
        /// Record observation epoch.
        observation_epoch: u64,
    },
    /// The record did not retain the expected namespace/disposition manifest.
    #[error("cancellation namespace-cleanup observation manifest does not match")]
    ManifestMismatch,
    /// The observation boundary is newer than current progress.
    #[error(
        "cancellation namespace-cleanup observation epoch {actual} is newer than current epoch {current}"
    )]
    ObservationEpochFuture {
        /// Epoch carried by the candidate record.
        actual: u64,
        /// Current progress execution epoch.
        current: u64,
    },
    /// The record did not cover every expected key exactly once.
    #[error(
        "cancellation namespace-cleanup observation row count {actual} does not match manifest count {expected}"
    )]
    RowCountMismatch {
        /// Number of supplied rows.
        actual: usize,
        /// Number of expected keys.
        expected: usize,
    },
    /// A row key did not match the canonical key at its position.
    #[error(
        "cancellation namespace-cleanup observation row {index} key {actual:?} does not match manifest key {expected:?}"
    )]
    RowKeyMismatch {
        /// Row position carrying the mismatch.
        index: usize,
        /// Supplied row key.
        actual: WorkUnitKey,
        /// Expected row key.
        expected: WorkUnitKey,
    },
}

impl CancellationNamespaceCleanupObservationRecord {
    /// Construct one untrusted namespace/disposition record after local shape validation.
    pub fn new(
        manifest: CancellationNamespaceCleanupManifest,
        observation_epoch: u64,
        observation_generation: u64,
        rows: Vec<CancellationNamespaceCleanupObservationRow>,
    ) -> Result<Self, CancellationNamespaceCleanupObservationRecordError> {
        let record = Self {
            manifest,
            observation_epoch,
            observation_generation,
            rows,
        };
        record.validate()?;
        Ok(record)
    }

    /// Validate local shape, role/outcome admissibility, nested fence shape, and epoch lineage.
    pub fn validate(&self) -> Result<(), CancellationNamespaceCleanupObservationRecordError> {
        self.manifest
            .validate()
            .map_err(CancellationNamespaceCleanupManifestError::from)
            .map_err(CancellationNamespaceCleanupObservationRecordError::Manifest)?;
        if self.observation_generation == 0 {
            return Err(
                CancellationNamespaceCleanupObservationRecordError::ObservationGenerationZero,
            );
        }
        let dispatch_epoch = self.manifest.dispatch_outcome().observation_epoch();
        if self.observation_epoch < dispatch_epoch {
            return Err(
                CancellationNamespaceCleanupObservationRecordError::ObservationEpochBeforeDispatch {
                    actual: self.observation_epoch,
                    dispatch: dispatch_epoch,
                },
            );
        }
        if self.rows.len() > MAX_CANCELLATION_NAMESPACE_CLEANUP_ROWS {
            return Err(
                CancellationNamespaceCleanupObservationRecordError::TooManyRows {
                    actual: self.rows.len(),
                    maximum: MAX_CANCELLATION_NAMESPACE_CLEANUP_ROWS,
                },
            );
        }
        for (index, row) in self.rows.iter().enumerate() {
            if index > 0 && self.rows[index - 1].key >= row.key {
                return Err(
                    CancellationNamespaceCleanupObservationRecordError::RowsNotSortedOrUnique {
                        index,
                    },
                );
            }
            let role = row.key.role();
            let (owner_revision, context) = match row.outcome() {
                CancellationNamespaceCleanupOutcome::Pending => continue,
                CancellationNamespaceCleanupOutcome::NoOperationClaim {
                    owner_revision,
                    context,
                } => {
                    if !allows_no_operation_claim(role) {
                        return Err(
                            CancellationNamespaceCleanupObservationRecordError::NoOperationClaimNotAllowed {
                                key: row.key,
                                role,
                            },
                        );
                    }
                    (*owner_revision, context)
                }
                CancellationNamespaceCleanupOutcome::Preserved {
                    owner_revision,
                    context,
                } => (*owner_revision, context),
                CancellationNamespaceCleanupOutcome::Absent {
                    owner_revision,
                    context,
                } => {
                    if !allows_absent(role) {
                        return Err(
                            CancellationNamespaceCleanupObservationRecordError::AbsentNotAllowed {
                                key: row.key,
                                role,
                            },
                        );
                    }
                    (*owner_revision, context)
                }
            };
            if owner_revision == 0 {
                return Err(
                    CancellationNamespaceCleanupObservationRecordError::OwnerRevisionZero {
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
                    CancellationNamespaceCleanupObservationRecordError::ContextScope {
                        key: row.key,
                        source,
                    }
                })?;
            if context.execution_epoch() > self.observation_epoch {
                return Err(
                    CancellationNamespaceCleanupObservationRecordError::ContextEpochAfterObservation {
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
        manifest: &CancellationNamespaceCleanupManifest,
        current_execution_epoch: u64,
    ) -> Result<(), CancellationNamespaceCleanupObservationRecordError> {
        self.validate()?;
        self.manifest
            .validate_against(manifest)
            .map_err(|_| CancellationNamespaceCleanupObservationRecordError::ManifestMismatch)?;
        if self.observation_epoch > current_execution_epoch {
            return Err(
                CancellationNamespaceCleanupObservationRecordError::ObservationEpochFuture {
                    actual: self.observation_epoch,
                    current: current_execution_epoch,
                },
            );
        }
        if self.rows.len() != manifest.keys().len() {
            return Err(
                CancellationNamespaceCleanupObservationRecordError::RowCountMismatch {
                    actual: self.rows.len(),
                    expected: manifest.keys().len(),
                },
            );
        }
        for (index, (expected, row)) in manifest.keys().iter().zip(&self.rows).enumerate() {
            if row.key != *expected {
                return Err(
                    CancellationNamespaceCleanupObservationRecordError::RowKeyMismatch {
                        index,
                        actual: row.key,
                        expected: *expected,
                    },
                );
            }
        }
        Ok(())
    }

    /// Borrow the exact namespace/disposition manifest bound to this record.
    pub fn manifest(&self) -> &CancellationNamespaceCleanupManifest {
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

    /// Borrow exact sorted namespace/disposition observation rows.
    pub fn rows(&self) -> &[CancellationNamespaceCleanupObservationRow] {
        &self.rows
    }
}
