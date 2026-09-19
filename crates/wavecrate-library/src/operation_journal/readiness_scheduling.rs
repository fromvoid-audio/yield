//! Bounded, non-authoritative observations for optional artifact readiness scheduling.
//!
//! The manifest keeps the planner's explicit Artifact-to-Readiness edges intact.  The record
//! carries the observations a future durable readiness owner will need, but neither type proves
//! that a wake was persisted, that the source is currently open, or that any optional artifact
//! may be deferred.  WorkProgress validates candidates against its current epoch and scopes;
//! accepting a validated observation as scheduling authority is a separate integration.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use thiserror::Error;
use uuid::Uuid;

use super::model::OperationId;
use super::participants::{
    MAX_PARTICIPANTS, ParticipantId, ParticipantKind, ParticipantRequirement,
    ParticipantValidationError,
};
use super::work_plan::{
    MAX_WORK_PLAN_UNITS, WorkPlan, WorkPlanError, WorkUnitApplicability, WorkUnitKey, WorkUnitRole,
};
use super::work_progress::{CompletionContext, ParticipantScope, SourceRevisionObservation};

/// Maximum UTF-8 bytes retained for one persisted readiness contract version.
pub const MAX_READINESS_CONTRACT_VERSION_BYTES: usize = 1024;

/// A fixed-width membership digest and its persisted nonnegative target count.
///
/// This describes the exact target set scheduled for one readiness source and selected artifact
/// set.  It must not be interpreted as a source-wide membership digest for an arbitrary subset.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReadinessTargetSetSummary {
    membership_digest: [u8; 32],
    membership_count: i64,
}

impl ReadinessTargetSetSummary {
    /// Construct a bounded readiness target-set summary.
    pub fn new(
        membership_digest: [u8; 32],
        membership_count: i64,
    ) -> Result<Self, ReadinessSchedulingObservationError> {
        if membership_count < 0 {
            return Err(
                ReadinessSchedulingObservationError::NegativeMembershipCount {
                    actual: membership_count,
                },
            );
        }
        Ok(Self {
            membership_digest,
            membership_count,
        })
    }

    /// Return the exact persisted membership digest.
    pub const fn membership_digest(&self) -> [u8; 32] {
        self.membership_digest
    }

    /// Return the persisted nonnegative target count.
    pub const fn membership_count(&self) -> i64 {
        self.membership_count
    }

    fn validate(&self) -> Result<(), ReadinessSchedulingObservationError> {
        if self.membership_count < 0 {
            return Err(
                ReadinessSchedulingObservationError::NegativeMembershipCount {
                    actual: self.membership_count,
                },
            );
        }
        Ok(())
    }
}

/// Caller-reported evidence that a durable readiness wake was observed.
///
/// The identity and owner revision are observations only.  They do not establish that a wake
/// was persisted or grant permission to enqueue work.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DurableReadinessWakeObservation {
    wake_id: Uuid,
    owner_revision: u64,
}

impl DurableReadinessWakeObservation {
    /// Construct one bounded wake observation.
    pub fn new(
        wake_id: Uuid,
        owner_revision: u64,
    ) -> Result<Self, ReadinessSchedulingObservationError> {
        if wake_id.is_nil() {
            return Err(ReadinessSchedulingObservationError::NilWakeId);
        }
        if owner_revision == 0 {
            return Err(ReadinessSchedulingObservationError::ZeroWakeOwnerRevision);
        }
        Ok(Self {
            wake_id,
            owner_revision,
        })
    }

    /// Return the opaque durable-wake identity observation.
    pub const fn wake_id(&self) -> Uuid {
        self.wake_id
    }

    /// Return the caller-reported owner revision.
    pub const fn owner_revision(&self) -> u64 {
        self.owner_revision
    }

    fn validate(&self) -> Result<(), ReadinessSchedulingObservationError> {
        if self.wake_id.is_nil() {
            return Err(ReadinessSchedulingObservationError::NilWakeId);
        }
        if self.owner_revision == 0 {
            return Err(ReadinessSchedulingObservationError::ZeroWakeOwnerRevision);
        }
        Ok(())
    }
}

/// One exact planner binding from an optional Artifact to its same-source Readiness prerequisite.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReadinessArtifactBinding {
    artifact_key: WorkUnitKey,
    readiness_key: WorkUnitKey,
}

impl ReadinessArtifactBinding {
    /// Construct one untrusted Artifact-to-Readiness binding.
    pub const fn new(artifact_key: WorkUnitKey, readiness_key: WorkUnitKey) -> Self {
        Self {
            artifact_key,
            readiness_key,
        }
    }

    /// Return the bound optional Artifact key.
    pub const fn artifact_key(self) -> WorkUnitKey {
        self.artifact_key
    }

    /// Return the bound same-source Readiness key.
    pub const fn readiness_key(self) -> WorkUnitKey {
        self.readiness_key
    }
}

/// Exact Artifact and direct Readiness prerequisite selection for one deferral observation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReadinessDeferralManifest {
    operation_id: OperationId,
    plan: WorkPlan,
    artifact_keys: Vec<WorkUnitKey>,
    bindings: Vec<ReadinessArtifactBinding>,
    readiness_keys: Vec<WorkUnitKey>,
}

impl ReadinessDeferralManifest {
    /// Return the operation identity bound to this manifest.
    pub const fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    /// Borrow the exact frozen plan bound to this manifest.
    pub fn plan(&self) -> &WorkPlan {
        &self.plan
    }

    /// Borrow the explicitly selected optional Artifact keys.
    pub fn artifact_keys(&self) -> &[WorkUnitKey] {
        &self.artifact_keys
    }

    /// Borrow the exact sorted Artifact-to-Readiness bindings.
    pub fn bindings(&self) -> &[ReadinessArtifactBinding] {
        &self.bindings
    }

    /// Borrow the sorted union of direct Readiness prerequisites.
    pub fn readiness_keys(&self) -> &[WorkUnitKey] {
        &self.readiness_keys
    }

    /// Validate exact equality with an explicitly re-derived expected manifest.
    pub fn validate_against(&self, expected: &Self) -> Result<(), ReadinessDeferralManifestError> {
        self.validate_shape()?;
        expected.validate_shape()?;
        if self != expected {
            return Err(ReadinessDeferralManifestError::ExpectedManifestMismatch);
        }
        Ok(())
    }

    fn from_canonical(
        operation_id: OperationId,
        plan: WorkPlan,
        artifact_keys: Vec<WorkUnitKey>,
        bindings: Vec<ReadinessArtifactBinding>,
        readiness_keys: Vec<WorkUnitKey>,
    ) -> Result<Self, ReadinessDeferralManifestError> {
        let manifest = Self {
            operation_id,
            plan,
            artifact_keys,
            bindings,
            readiness_keys,
        };
        manifest.validate_shape()?;
        Ok(manifest)
    }

    fn validate_shape(&self) -> Result<(), ReadinessDeferralManifestError> {
        if self.operation_id.as_uuid().is_nil() {
            return Err(ReadinessDeferralManifestError::InvalidOperationId);
        }
        self.plan.validate()?;
        validate_sorted_unique(
            &self.artifact_keys,
            MAX_WORK_PLAN_UNITS,
            ReadinessDeferralManifestError::ArtifactKeysNotSortedUnique,
            |actual, maximum| ReadinessDeferralManifestError::TooManyArtifactKeys {
                actual,
                maximum,
            },
        )?;
        if self.artifact_keys.is_empty() {
            return Err(ReadinessDeferralManifestError::EmptyArtifactSelection);
        }
        if self.bindings.len() != self.artifact_keys.len() {
            return Err(ReadinessDeferralManifestError::BindingCoverageMismatch {
                expected: self.artifact_keys.len(),
                actual: self.bindings.len(),
            });
        }
        if self
            .bindings
            .windows(2)
            .any(|window| window[0].artifact_key() >= window[1].artifact_key())
        {
            return Err(ReadinessDeferralManifestError::BindingsNotSortedUnique);
        }
        validate_sorted_unique(
            &self.readiness_keys,
            MAX_WORK_PLAN_UNITS,
            ReadinessDeferralManifestError::ReadinessKeysNotSortedUnique,
            |actual, maximum| ReadinessDeferralManifestError::TooManyReadinessKeys {
                actual,
                maximum,
            },
        )?;
        if self.readiness_keys.is_empty() {
            return Err(
                ReadinessDeferralManifestError::ReadinessPrerequisitesMissing {
                    artifact: self.artifact_keys[0],
                },
            );
        }

        for (index, artifact) in self.artifact_keys.iter().enumerate() {
            let unit = self
                .plan
                .units()
                .iter()
                .find(|unit| unit.key() == *artifact)
                .ok_or(ReadinessDeferralManifestError::UnknownArtifact { key: *artifact })?;
            if unit.key().role() != WorkUnitRole::Artifact {
                return Err(ReadinessDeferralManifestError::SelectedUnitNotArtifact {
                    key: *artifact,
                });
            }
            if unit.applicability() != WorkUnitApplicability::Required {
                return Err(ReadinessDeferralManifestError::ArtifactNotApplicable {
                    key: *artifact,
                });
            }
            if unit.requirement() != ParticipantRequirement::OptionalRebuildable {
                return Err(
                    ReadinessDeferralManifestError::ArtifactRequirementMismatch { key: *artifact },
                );
            }
            let binding = self.bindings[index];
            if binding.artifact_key() != *artifact {
                return Err(ReadinessDeferralManifestError::BindingArtifactMismatch {
                    key: *artifact,
                });
            }
            let readiness = self
                .plan
                .units()
                .iter()
                .find(|candidate| candidate.key() == binding.readiness_key())
                .ok_or(ReadinessDeferralManifestError::UnknownReadiness {
                    key: binding.readiness_key(),
                })?;
            if readiness.key().role() != WorkUnitRole::Readiness
                || readiness.applicability() != WorkUnitApplicability::Required
                || readiness.requirement() != ParticipantRequirement::Required
            {
                return Err(ReadinessDeferralManifestError::ReadinessNotRequired {
                    key: binding.readiness_key(),
                });
            }
            if !self.plan.prerequisites().iter().any(|edge| {
                edge.prerequisite() == binding.readiness_key() && edge.dependent() == *artifact
            }) {
                return Err(
                    ReadinessDeferralManifestError::BindingNotDirectPrerequisite {
                        artifact: *artifact,
                        readiness: binding.readiness_key(),
                    },
                );
            }
        }

        for readiness in &self.readiness_keys {
            let unit = self
                .plan
                .units()
                .iter()
                .find(|unit| unit.key() == *readiness)
                .ok_or(ReadinessDeferralManifestError::UnknownReadiness { key: *readiness })?;
            if unit.key().role() != WorkUnitRole::Readiness
                || unit.applicability() != WorkUnitApplicability::Required
                || unit.requirement() != ParticipantRequirement::Required
            {
                return Err(ReadinessDeferralManifestError::ReadinessNotRequired {
                    key: *readiness,
                });
            }
            if !self
                .bindings
                .iter()
                .any(|binding| binding.readiness_key() == *readiness)
            {
                return Err(
                    ReadinessDeferralManifestError::ReadinessNotSelectedForArtifact {
                        key: *readiness,
                    },
                );
            }
        }
        Ok(())
    }
}

/// Derive the canonical manifest for an explicit nonempty optional-artifact selection.
pub fn derive_readiness_deferral_manifest(
    operation_id: OperationId,
    plan: &WorkPlan,
    scopes: &[ParticipantScope],
    selected_artifact_keys: &[WorkUnitKey],
) -> Result<ReadinessDeferralManifest, ReadinessDeferralManifestError> {
    if operation_id.as_uuid().is_nil() {
        return Err(ReadinessDeferralManifestError::InvalidOperationId);
    }
    plan.validate()?;
    validate_sorted_unique(
        selected_artifact_keys,
        MAX_WORK_PLAN_UNITS,
        ReadinessDeferralManifestError::ArtifactKeysNotSortedUnique,
        |actual, maximum| ReadinessDeferralManifestError::TooManyArtifactKeys { actual, maximum },
    )?;
    if selected_artifact_keys.is_empty() {
        return Err(ReadinessDeferralManifestError::EmptyArtifactSelection);
    }
    validate_scope_collection(scopes)?;

    let units = plan.units();
    let mut bindings = Vec::with_capacity(selected_artifact_keys.len());
    let mut readiness_keys = BTreeSet::new();
    for artifact in selected_artifact_keys {
        let unit = units
            .iter()
            .find(|unit| unit.key() == *artifact)
            .ok_or(ReadinessDeferralManifestError::UnknownArtifact { key: *artifact })?;
        if unit.key().role() != WorkUnitRole::Artifact {
            return Err(ReadinessDeferralManifestError::SelectedUnitNotArtifact { key: *artifact });
        }
        if unit.applicability() != WorkUnitApplicability::Required {
            return Err(ReadinessDeferralManifestError::ArtifactNotApplicable { key: *artifact });
        }
        if unit.requirement() != ParticipantRequirement::OptionalRebuildable {
            return Err(
                ReadinessDeferralManifestError::ArtifactRequirementMismatch { key: *artifact },
            );
        }
        let artifact_scope = scopes
            .iter()
            .find(|scope| scope.participant_id() == artifact.participant_id())
            .ok_or(ReadinessDeferralManifestError::ParticipantScopeMissing {
                participant_id: artifact.participant_id(),
            })?;
        if artifact_scope.kind() != ParticipantKind::Artifact {
            return Err(ReadinessDeferralManifestError::ScopeKindMismatch {
                participant_id: artifact.participant_id(),
                expected: ParticipantKind::Artifact,
                actual: artifact_scope.kind(),
            });
        }
        let artifact_fence = artifact_scope
            .source_fence()
            .ok_or(ReadinessDeferralManifestError::ArtifactSourceFenceMissing { key: *artifact })?;
        artifact_fence
            .validate()
            .map_err(ReadinessDeferralManifestError::ScopeFenceInvalid)?;
        let mut matching_readiness = None;
        for edge in plan.prerequisites() {
            if edge.dependent() != *artifact {
                continue;
            }
            let Some(prerequisite) = units.iter().find(|unit| unit.key() == edge.prerequisite())
            else {
                return Err(ReadinessDeferralManifestError::Plan(
                    WorkPlanError::EdgeEndpointMissing { edge: *edge },
                ));
            };
            if prerequisite.key().role() == WorkUnitRole::Readiness
                && prerequisite.applicability() == WorkUnitApplicability::Required
                && prerequisite.requirement() == ParticipantRequirement::Required
            {
                let readiness_scope = scopes
                    .iter()
                    .find(|scope| scope.participant_id() == prerequisite.key().participant_id())
                    .ok_or(ReadinessDeferralManifestError::ParticipantScopeMissing {
                        participant_id: prerequisite.key().participant_id(),
                    })?;
                if readiness_scope.kind() != ParticipantKind::Readiness {
                    return Err(ReadinessDeferralManifestError::ScopeKindMismatch {
                        participant_id: prerequisite.key().participant_id(),
                        expected: ParticipantKind::Readiness,
                        actual: readiness_scope.kind(),
                    });
                }
                let readiness_fence = readiness_scope.source_fence().ok_or(
                    ReadinessDeferralManifestError::ReadinessSourceFenceMissing {
                        key: prerequisite.key(),
                    },
                )?;
                readiness_fence
                    .validate()
                    .map_err(ReadinessDeferralManifestError::ScopeFenceInvalid)?;
                if readiness_scope.source_fence() == Some(artifact_fence) {
                    if matching_readiness.replace(prerequisite.key()).is_some() {
                        return Err(
                            ReadinessDeferralManifestError::AmbiguousReadinessPrerequisites {
                                artifact: *artifact,
                            },
                        );
                    }
                }
            }
        }
        let readiness = matching_readiness.ok_or(
            ReadinessDeferralManifestError::ReadinessPrerequisitesMissing {
                artifact: *artifact,
            },
        )?;
        bindings.push(ReadinessArtifactBinding::new(*artifact, readiness));
        readiness_keys.insert(readiness);
        if bindings.len() > MAX_WORK_PLAN_UNITS {
            return Err(ReadinessDeferralManifestError::TooManyBindings {
                actual: bindings.len(),
                maximum: MAX_WORK_PLAN_UNITS,
            });
        }
    }
    if readiness_keys.len() > MAX_WORK_PLAN_UNITS {
        return Err(ReadinessDeferralManifestError::TooManyReadinessKeys {
            actual: readiness_keys.len(),
            maximum: MAX_WORK_PLAN_UNITS,
        });
    }
    ReadinessDeferralManifest::from_canonical(
        operation_id,
        plan.clone(),
        selected_artifact_keys.to_vec(),
        bindings,
        readiness_keys.into_iter().collect(),
    )
}

fn validate_scope_collection(
    scopes: &[ParticipantScope],
) -> Result<(), ReadinessDeferralManifestError> {
    if scopes.len() > MAX_PARTICIPANTS {
        return Err(ReadinessDeferralManifestError::TooManyScopes {
            actual: scopes.len(),
            maximum: MAX_PARTICIPANTS,
        });
    }
    let mut participant_ids = BTreeSet::new();
    for scope in scopes {
        if !participant_ids.insert(scope.participant_id()) {
            return Err(ReadinessDeferralManifestError::DuplicateParticipantScope {
                participant_id: scope.participant_id(),
            });
        }
    }
    Ok(())
}

/// One exact readiness prerequisite observation needed for the selected artifact set.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReadinessSchedulingObservationRow {
    readiness_key: WorkUnitKey,
    context: CompletionContext,
    source_observation: SourceRevisionObservation,
    source_generation: i64,
    readiness_revision: i64,
    contract_version: String,
    target_set: ReadinessTargetSetSummary,
    wake: DurableReadinessWakeObservation,
}

impl ReadinessSchedulingObservationRow {
    /// Construct one untrusted current-readiness scheduling observation row.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        readiness_key: WorkUnitKey,
        context: CompletionContext,
        source_observation: SourceRevisionObservation,
        source_generation: i64,
        readiness_revision: i64,
        contract_version: impl AsRef<str>,
        target_set: ReadinessTargetSetSummary,
        wake: DurableReadinessWakeObservation,
    ) -> Result<Self, ReadinessSchedulingObservationError> {
        let row = Self {
            readiness_key,
            context,
            source_observation,
            source_generation,
            readiness_revision,
            contract_version: contract_version.as_ref().to_owned(),
            target_set,
            wake,
        };
        row.validate_shape()?;
        Ok(row)
    }

    /// Return the readiness work-unit key.
    pub const fn readiness_key(&self) -> WorkUnitKey {
        self.readiness_key
    }

    /// Borrow the current completion context observation.
    pub fn context(&self) -> &CompletionContext {
        &self.context
    }

    /// Borrow the source-fence revision observation.
    pub fn source_observation(&self) -> &SourceRevisionObservation {
        &self.source_observation
    }

    /// Return the readiness source membership generation.
    pub const fn source_generation(&self) -> i64 {
        self.source_generation
    }

    /// Return the readiness publication revision.
    pub const fn readiness_revision(&self) -> i64 {
        self.readiness_revision
    }

    /// Borrow the exact persisted readiness contract version.
    pub fn contract_version(&self) -> &str {
        &self.contract_version
    }

    /// Borrow the exact target-set summary.
    pub const fn target_set(&self) -> &ReadinessTargetSetSummary {
        &self.target_set
    }

    /// Borrow the caller-reported durable wake observation.
    pub const fn wake(&self) -> &DurableReadinessWakeObservation {
        &self.wake
    }

    fn validate_shape(&self) -> Result<(), ReadinessSchedulingObservationError> {
        if self.readiness_key.role() != WorkUnitRole::Readiness {
            return Err(ReadinessSchedulingObservationError::RowNotReadiness {
                key: self.readiness_key,
            });
        }
        self.source_observation.validate()?;
        if self.source_generation < 0 {
            return Err(
                ReadinessSchedulingObservationError::NegativeSourceGeneration {
                    actual: self.source_generation,
                },
            );
        }
        if self.readiness_revision < 0 {
            return Err(
                ReadinessSchedulingObservationError::NegativeReadinessRevision {
                    actual: self.readiness_revision,
                },
            );
        }
        if self.contract_version.trim().is_empty() {
            return Err(ReadinessSchedulingObservationError::EmptyContractVersion);
        }
        if self.contract_version.len() > MAX_READINESS_CONTRACT_VERSION_BYTES {
            return Err(
                ReadinessSchedulingObservationError::ContractVersionTooLong {
                    actual: self.contract_version.len(),
                    maximum: MAX_READINESS_CONTRACT_VERSION_BYTES,
                },
            );
        }
        self.target_set.validate()?;
        self.wake.validate()?;
        Ok(())
    }
}

/// Exact current-epoch observations for all readiness prerequisites in a deferral manifest.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReadinessSchedulingObservationRecord {
    manifest: ReadinessDeferralManifest,
    observation_epoch: u64,
    observation_generation: u64,
    rows: Vec<ReadinessSchedulingObservationRow>,
}

impl ReadinessSchedulingObservationRecord {
    /// Construct an untrusted readiness scheduling record.
    pub fn new(
        manifest: ReadinessDeferralManifest,
        observation_epoch: u64,
        observation_generation: u64,
        rows: Vec<ReadinessSchedulingObservationRow>,
    ) -> Result<Self, ReadinessSchedulingObservationError> {
        let record = Self {
            manifest,
            observation_epoch,
            observation_generation,
            rows,
        };
        record.validate_shape()?;
        Ok(record)
    }

    /// Borrow the exact manifest bound to this record.
    pub fn manifest(&self) -> &ReadinessDeferralManifest {
        &self.manifest
    }

    /// Return the owner-reported current observation epoch.
    pub const fn observation_epoch(&self) -> u64 {
        self.observation_epoch
    }

    /// Return the positive readiness observation generation.
    pub const fn observation_generation(&self) -> u64 {
        self.observation_generation
    }

    /// Borrow all exact sorted readiness rows.
    pub fn rows(&self) -> &[ReadinessSchedulingObservationRow] {
        &self.rows
    }

    /// Validate shape and exact equality against an explicitly expected manifest.
    pub fn validate_against(
        &self,
        expected_manifest: &ReadinessDeferralManifest,
    ) -> Result<(), ReadinessSchedulingObservationError> {
        self.validate_shape()?;
        expected_manifest
            .validate_shape()
            .map_err(ReadinessSchedulingObservationError::Manifest)?;
        self.manifest
            .validate_against(expected_manifest)
            .map_err(ReadinessSchedulingObservationError::Manifest)
    }

    fn validate_shape(&self) -> Result<(), ReadinessSchedulingObservationError> {
        self.manifest
            .validate_shape()
            .map_err(ReadinessSchedulingObservationError::Manifest)?;
        if self.observation_generation == 0 {
            return Err(ReadinessSchedulingObservationError::ZeroObservationGeneration);
        }
        if self.rows.len() != self.manifest.readiness_keys.len() {
            return Err(ReadinessSchedulingObservationError::RowCoverageMismatch {
                expected: self.manifest.readiness_keys.len(),
                actual: self.rows.len(),
            });
        }
        let mut wake_ids = BTreeSet::new();
        for (index, row) in self.rows.iter().enumerate() {
            row.validate_shape()?;
            if index > 0 && self.rows[index - 1].readiness_key >= row.readiness_key {
                return Err(ReadinessSchedulingObservationError::RowsNotSortedUnique);
            }
            if !self.manifest.readiness_keys.contains(&row.readiness_key) {
                return Err(
                    ReadinessSchedulingObservationError::UnexpectedReadinessRow {
                        key: row.readiness_key,
                    },
                );
            }
            if !wake_ids.insert(row.wake.wake_id) {
                return Err(ReadinessSchedulingObservationError::DuplicateWakeId {
                    wake_id: row.wake.wake_id,
                });
            }
        }
        if self
            .manifest
            .readiness_keys
            .iter()
            .zip(self.rows.iter().map(|row| row.readiness_key))
            .any(|(expected, actual)| expected != &actual)
        {
            return Err(ReadinessSchedulingObservationError::RowCoverageMismatch {
                expected: self.manifest.readiness_keys.len(),
                actual: self.rows.len(),
            });
        }
        Ok(())
    }
}

fn validate_sorted_unique(
    keys: &[WorkUnitKey],
    maximum: usize,
    not_sorted: ReadinessDeferralManifestError,
    too_many: fn(usize, usize) -> ReadinessDeferralManifestError,
) -> Result<(), ReadinessDeferralManifestError> {
    if keys.len() > maximum {
        return Err(too_many(keys.len(), maximum));
    }
    if keys.windows(2).any(|window| window[0] >= window[1]) {
        return Err(not_sorted);
    }
    Ok(())
}

/// Failure while deriving or validating a readiness deferral manifest.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum ReadinessDeferralManifestError {
    /// The operation identity was nil.
    #[error("readiness deferral operation identity must be non-nil")]
    InvalidOperationId,
    /// The frozen plan was malformed.
    #[error("invalid readiness deferral plan: {0}")]
    Plan(#[source] WorkPlanError),
    /// The explicit Artifact selection was empty.
    #[error("readiness deferral Artifact selection must not be empty")]
    EmptyArtifactSelection,
    /// The explicit Artifact selection exceeded the frozen-plan bound.
    #[error("readiness deferral Artifact selection has {actual} keys, maximum is {maximum}")]
    TooManyArtifactKeys {
        /// Number of supplied Artifact keys.
        actual: usize,
        /// Maximum number of plan units.
        maximum: usize,
    },
    /// The binding count did not cover every selected Artifact.
    #[error("readiness deferral binding coverage is {actual}, expected {expected}")]
    BindingCoverageMismatch {
        /// Number of selected Artifact keys.
        expected: usize,
        /// Number of supplied bindings.
        actual: usize,
    },
    /// Bindings were not sorted uniquely by Artifact key.
    #[error("readiness deferral bindings must be sorted and unique")]
    BindingsNotSortedUnique,
    /// Artifact keys were not strictly sorted and unique.
    #[error("readiness deferral Artifact keys must be sorted and unique")]
    ArtifactKeysNotSortedUnique,
    /// Readiness keys exceeded the frozen-plan bound.
    #[error("readiness deferral Readiness selection has {actual} keys, maximum is {maximum}")]
    TooManyReadinessKeys {
        /// Number of derived Readiness keys.
        actual: usize,
        /// Maximum number of plan units.
        maximum: usize,
    },
    /// Readiness keys were not strictly sorted and unique.
    #[error("readiness deferral Readiness keys must be sorted and unique")]
    ReadinessKeysNotSortedUnique,
    /// A selected key was absent from the plan.
    #[error("readiness deferral selected unknown Artifact {key:?}")]
    UnknownArtifact {
        /// Unknown selected key.
        key: WorkUnitKey,
    },
    /// A selected key was not an Artifact role.
    #[error("readiness deferral selected key is not an Artifact: {key:?}")]
    SelectedUnitNotArtifact {
        /// Selected key carrying another role.
        key: WorkUnitKey,
    },
    /// A selected Artifact was not applicable.
    #[error("readiness deferral Artifact is not applicable: {key:?}")]
    ArtifactNotApplicable {
        /// Selected Artifact key.
        key: WorkUnitKey,
    },
    /// A selected Artifact was not optional-rebuildable.
    #[error("readiness deferral Artifact is not optional-rebuildable: {key:?}")]
    ArtifactRequirementMismatch {
        /// Selected Artifact key.
        key: WorkUnitKey,
    },
    /// The bounded source scope for an Artifact was unavailable.
    #[error("readiness deferral Artifact has no source fence: {key:?}")]
    ArtifactSourceFenceMissing {
        /// Artifact key lacking a source fence.
        key: WorkUnitKey,
    },
    /// A participant scope required by the current plan was unavailable.
    #[error("readiness deferral participant scope is missing: {participant_id}")]
    ParticipantScopeMissing {
        /// Participant whose scope was absent.
        participant_id: ParticipantId,
    },
    /// The supplied participant scope collection exceeded the bounded participant limit.
    #[error(
        "readiness deferral participant scope collection has {actual} entries, maximum is {maximum}"
    )]
    TooManyScopes {
        /// Number of supplied participant scopes.
        actual: usize,
        /// Maximum number of participant scopes.
        maximum: usize,
    },
    /// The supplied participant scope collection repeated one participant identity.
    #[error("readiness deferral participant scope is duplicated: {participant_id}")]
    DuplicateParticipantScope {
        /// Repeated participant identity.
        participant_id: ParticipantId,
    },
    /// A participant scope used by the selected binding had the wrong role kind.
    #[error(
        "readiness deferral participant {participant_id} has kind {actual:?}, expected {expected:?}"
    )]
    ScopeKindMismatch {
        /// Participant whose role kind was invalid.
        participant_id: ParticipantId,
        /// Required role kind.
        expected: ParticipantKind,
        /// Supplied role kind.
        actual: ParticipantKind,
    },
    /// A participant scope used by the selected binding carried an invalid source fence.
    #[error("readiness deferral participant source fence is invalid: {0}")]
    ScopeFenceInvalid(#[source] ParticipantValidationError),
    /// A same-source Readiness prerequisite had no current source fence.
    #[error("readiness deferral Readiness has no source fence: {key:?}")]
    ReadinessSourceFenceMissing {
        /// Readiness key lacking a source fence.
        key: WorkUnitKey,
    },
    /// More than one same-source Readiness prerequisite matched one Artifact.
    #[error(
        "readiness deferral Artifact has ambiguous same-source Readiness prerequisites: {artifact:?}"
    )]
    AmbiguousReadinessPrerequisites {
        /// Artifact with multiple matching prerequisites.
        artifact: WorkUnitKey,
    },
    /// A binding did not preserve its selected Artifact key.
    #[error("readiness deferral binding Artifact does not match selection: {key:?}")]
    BindingArtifactMismatch {
        /// Selected Artifact key carrying the mismatch.
        key: WorkUnitKey,
    },
    /// A binding did not correspond to a direct plan prerequisite edge.
    #[error(
        "readiness deferral binding is not a direct prerequisite: {artifact:?} <- {readiness:?}"
    )]
    BindingNotDirectPrerequisite {
        /// Artifact endpoint of the invalid binding.
        artifact: WorkUnitKey,
        /// Readiness endpoint of the invalid binding.
        readiness: WorkUnitKey,
    },
    /// A selected Artifact had no explicit applicable Readiness prerequisite.
    #[error("readiness deferral Artifact has no applicable Readiness prerequisite: {artifact:?}")]
    ReadinessPrerequisitesMissing {
        /// Artifact lacking a direct Readiness prerequisite.
        artifact: WorkUnitKey,
    },
    /// A Readiness key was absent from the plan.
    #[error("readiness deferral selected unknown Readiness {key:?}")]
    UnknownReadiness {
        /// Unknown Readiness key.
        key: WorkUnitKey,
    },
    /// A Readiness key was not applicable and required.
    #[error("readiness deferral Readiness is not required and applicable: {key:?}")]
    ReadinessNotRequired {
        /// Readiness key with the wrong applicability.
        key: WorkUnitKey,
    },
    /// A Readiness key did not directly serve a selected Artifact.
    #[error("readiness deferral Readiness is not a prerequisite of a selected Artifact: {key:?}")]
    ReadinessNotSelectedForArtifact {
        /// Readiness key not linked to the selected set.
        key: WorkUnitKey,
    },
    /// The Artifact-to-Readiness binding count exceeded the plan bound.
    #[error("readiness deferral has {actual} bindings, maximum is {maximum}")]
    TooManyBindings {
        /// Number of bindings.
        actual: usize,
        /// Maximum number of plan units.
        maximum: usize,
    },
    /// A candidate manifest differed from an explicit expected derivation.
    #[error("readiness deferral manifest does not match the explicit expected selection")]
    ExpectedManifestMismatch,
}

impl From<WorkPlanError> for ReadinessDeferralManifestError {
    fn from(error: WorkPlanError) -> Self {
        Self::Plan(error)
    }
}

/// Failure while validating one readiness scheduling observation.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum ReadinessSchedulingObservationError {
    /// A manifest failed local validation or exact expected binding.
    #[error("invalid readiness deferral manifest: {0}")]
    Manifest(#[source] ReadinessDeferralManifestError),
    /// The persisted membership count was negative.
    #[error("readiness target-set membership count {actual} is negative")]
    NegativeMembershipCount {
        /// Persisted count supplied by the observation.
        actual: i64,
    },
    /// A wake identity was nil.
    #[error("readiness durable wake identity must be non-nil")]
    NilWakeId,
    /// A wake owner revision was zero.
    #[error("readiness durable wake owner revision must be positive")]
    ZeroWakeOwnerRevision,
    /// The readiness row named another role.
    #[error("readiness scheduling row is not a Readiness unit: {key:?}")]
    RowNotReadiness {
        /// Key supplied by the row.
        key: WorkUnitKey,
    },
    /// The persisted source generation was negative.
    #[error("readiness source generation {actual} is negative")]
    NegativeSourceGeneration {
        /// Persisted source generation supplied by the row.
        actual: i64,
    },
    /// The persisted readiness revision was negative.
    #[error("readiness revision {actual} is negative")]
    NegativeReadinessRevision {
        /// Persisted readiness revision supplied by the row.
        actual: i64,
    },
    /// The persisted readiness contract version was empty.
    #[error("readiness contract version must be non-empty")]
    EmptyContractVersion,
    /// The persisted readiness contract version exceeded its bounded text limit.
    #[error("readiness contract version has {actual} bytes, maximum is {maximum}")]
    ContractVersionTooLong {
        /// Number of UTF-8 bytes supplied by the row.
        actual: usize,
        /// Maximum accepted UTF-8 bytes.
        maximum: usize,
    },
    /// The observation generation was zero.
    #[error("readiness scheduling observation generation must be positive")]
    ZeroObservationGeneration,
    /// Rows were not strictly sorted and unique.
    #[error("readiness scheduling rows must be sorted and unique")]
    RowsNotSortedUnique,
    /// The row count or exact row keys did not match the manifest.
    #[error("readiness scheduling row coverage is {actual}, expected {expected}")]
    RowCoverageMismatch {
        /// Number of expected readiness rows.
        expected: usize,
        /// Number of supplied readiness rows.
        actual: usize,
    },
    /// A row named a key not present in the manifest.
    #[error("readiness scheduling row names unexpected key {key:?}")]
    UnexpectedReadinessRow {
        /// Unexpected readiness key.
        key: WorkUnitKey,
    },
    /// Two readiness rows reused one durable wake identity.
    #[error("readiness scheduling rows reused wake identity {wake_id}")]
    DuplicateWakeId {
        /// Wake identity reused by another row.
        wake_id: Uuid,
    },
    /// A source revision observation was malformed.
    #[error("invalid readiness source revision observation: {0}")]
    SourceObservation(#[source] super::work_progress::ProgressEvidenceError),
}

impl From<super::work_progress::ProgressEvidenceError> for ReadinessSchedulingObservationError {
    fn from(error: super::work_progress::ProgressEvidenceError) -> Self {
        Self::SourceObservation(error)
    }
}
