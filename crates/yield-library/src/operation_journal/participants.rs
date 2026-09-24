//! Pure stored participant descriptors and checkpoints for universal operations.
//!
//! These values are untrusted observations. They contain no live handles, ownership proofs,
//! filesystem paths, or permission to advance a journal phase. Constructors and validators only
//! check the bounded stored shape; a later coordinator must establish live ownership before it
//! can act on any participant.

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::sample_sources::SourceId;

/// Maximum number of participant descriptors or checkpoints in one operation record.
pub const MAX_PARTICIPANTS: usize = 64;
/// Maximum byte length of a source, root, or database identity string.
pub const MAX_SOURCE_IDENTITY_BYTES: usize = 256;
/// Maximum byte length of an opaque rebuildable-artifact key or reference.
pub const MAX_ARTIFACT_REFERENCE_BYTES: usize = 1024;
/// Maximum byte length of a stable participant diagnostic code.
pub const MAX_DIAGNOSTIC_CODE_BYTES: usize = 128;

/// Opaque UUID identity for one stored operation participant.
///
/// Creating or decoding an ID does not establish ownership or durable admission.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ParticipantId(Uuid);

impl ParticipantId {
    /// Generate a new participant identity.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Wrap an existing UUID without interpreting it as live authority.
    pub fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    /// Parse a participant identity from a UUID spelling.
    pub fn parse(value: &str) -> Result<Self, uuid::Error> {
        Uuid::parse_str(value).map(Self)
    }

    /// Borrow the wrapped UUID value.
    pub fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for ParticipantId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ParticipantId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Universal operation participant family.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ParticipantKind {
    /// Filesystem mutation or publication participant.
    Filesystem,
    /// Source-database participant.
    SourceDatabase,
    /// Profile-global library participant.
    GlobalLibrary,
    /// Metadata harvest participant.
    Harvest,
    /// UI or catalog projection participant.
    Projection,
    /// Readiness scheduling participant.
    Readiness,
    /// Rebuildable artifact participant.
    Artifact,
}

impl ParticipantKind {
    fn requires_source_fence(self) -> bool {
        matches!(
            self,
            Self::SourceDatabase | Self::Projection | Self::Readiness | Self::Artifact
        )
    }
}

/// Durability requirement for one participant.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ParticipantRequirement {
    /// The participant must complete before a successful terminal result.
    Required,
    /// The participant can be rebuilt after the operation while remaining deferred.
    OptionalRebuildable,
}

/// Stored observation of the source ownership boundary relevant to a participant.
///
/// The identities are opaque values captured by an owner. They are not paths and are not
/// sufficient to reacquire a source database or lease.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StoredSourceFence {
    source_id: SourceId,
    root_identity: String,
    database_identity: String,
    lifecycle_generation: u64,
    source_revision: u64,
    lease_epoch: u64,
}

impl StoredSourceFence {
    /// Create a bounded source-fence observation.
    pub fn new(
        source_id: SourceId,
        root_identity: impl Into<String>,
        database_identity: impl Into<String>,
        lifecycle_generation: u64,
        source_revision: u64,
        lease_epoch: u64,
    ) -> Result<Self, ParticipantValidationError> {
        let fence = Self {
            source_id,
            root_identity: root_identity.into(),
            database_identity: database_identity.into(),
            lifecycle_generation,
            source_revision,
            lease_epoch,
        };
        fence.validate()?;
        Ok(fence)
    }

    /// Validate this retained source observation without acquiring any live resource.
    pub fn validate(&self) -> Result<(), ParticipantValidationError> {
        validate_text(
            self.source_id.as_str(),
            ParticipantTextField::SourceId,
            MAX_SOURCE_IDENTITY_BYTES,
        )?;
        validate_text(
            &self.root_identity,
            ParticipantTextField::RootIdentity,
            MAX_SOURCE_IDENTITY_BYTES,
        )?;
        validate_text(
            &self.database_identity,
            ParticipantTextField::DatabaseIdentity,
            MAX_SOURCE_IDENTITY_BYTES,
        )?;
        if self.lease_epoch == 0 {
            return Err(ParticipantValidationError::InvalidLeaseEpoch);
        }
        Ok(())
    }

    /// Return the stored source identifier.
    pub fn source_id(&self) -> &SourceId {
        &self.source_id
    }

    /// Return the stored root identity observation.
    pub fn root_identity(&self) -> &str {
        &self.root_identity
    }

    /// Return the stored source-database identity observation.
    pub fn database_identity(&self) -> &str {
        &self.database_identity
    }

    /// Return the stored source lifecycle generation.
    pub const fn lifecycle_generation(&self) -> u64 {
        self.lifecycle_generation
    }

    /// Return the stored source revision.
    pub const fn source_revision(&self) -> u64 {
        self.source_revision
    }

    /// Return the stored source lease epoch.
    pub const fn lease_epoch(&self) -> u64 {
        self.lease_epoch
    }
}

/// Stored descriptor for one universal operation participant.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ParticipantDescriptor {
    id: ParticipantId,
    kind: ParticipantKind,
    requirement: ParticipantRequirement,
    source_fence: Option<StoredSourceFence>,
    artifact_key: Option<String>,
}

impl ParticipantDescriptor {
    /// Create a descriptor after checking kind, requirement, and stored-evidence relationships.
    pub fn new(
        id: ParticipantId,
        kind: ParticipantKind,
        requirement: ParticipantRequirement,
        source_fence: Option<StoredSourceFence>,
        artifact_key: Option<String>,
    ) -> Result<Self, ParticipantValidationError> {
        let descriptor = Self {
            id,
            kind,
            requirement,
            source_fence,
            artifact_key,
        };
        descriptor.validate()?;
        Ok(descriptor)
    }

    /// Validate this descriptor as untrusted stored data.
    pub fn validate(&self) -> Result<(), ParticipantValidationError> {
        if let Some(source_fence) = &self.source_fence {
            source_fence.validate()?;
        }
        if self.kind.requires_source_fence() && self.source_fence.is_none() {
            return Err(ParticipantValidationError::SourceFenceRequired { kind: self.kind });
        }
        if self.kind == ParticipantKind::GlobalLibrary && self.source_fence.is_some() {
            return Err(ParticipantValidationError::GlobalLibrarySourceFenceForbidden);
        }
        if self.requirement == ParticipantRequirement::OptionalRebuildable
            && self.kind != ParticipantKind::Artifact
        {
            return Err(ParticipantValidationError::OptionalRequirementOnlyArtifact);
        }
        match (self.kind, &self.artifact_key) {
            (ParticipantKind::Artifact, None) => {
                Err(ParticipantValidationError::ArtifactKeyRequired)
            }
            (ParticipantKind::Artifact, Some(key)) => {
                validate_text(
                    key,
                    ParticipantTextField::ArtifactKey,
                    MAX_ARTIFACT_REFERENCE_BYTES,
                )?;
                Ok(())
            }
            (_, Some(_)) => {
                Err(ParticipantValidationError::ArtifactKeyForbidden { kind: self.kind })
            }
            (_, None) => Ok(()),
        }
    }

    /// Return this participant's opaque ID.
    pub const fn id(&self) -> ParticipantId {
        self.id
    }

    /// Return this participant's kind.
    pub const fn kind(&self) -> ParticipantKind {
        self.kind
    }

    /// Return this participant's durability requirement.
    pub const fn requirement(&self) -> ParticipantRequirement {
        self.requirement
    }

    /// Return the stored source-fence observation, when present.
    pub fn source_fence(&self) -> Option<&StoredSourceFence> {
        self.source_fence.as_ref()
    }

    /// Return the stored opaque artifact key or reference, when present.
    pub fn artifact_key(&self) -> Option<&str> {
        self.artifact_key.as_deref()
    }
}

/// Stored state observation for one participant checkpoint.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ParticipantCheckpointState {
    /// Work for the participant has not completed.
    Pending,
    /// The participant owner recorded successful application.
    Applied,
    /// The participant was not applicable to this operation.
    NotApplicable,
    /// Optional rebuildable work was deliberately left for later.
    Deferred,
    /// The participant can be retried from retained evidence.
    RetryPending,
    /// The participant evidence needs an explicit audit.
    AuditRequired,
}

/// Stored checkpoint observation for one participant.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ParticipantCheckpoint {
    participant_id: ParticipantId,
    state: ParticipantCheckpointState,
    committed_revision: Option<u64>,
    diagnostic_code: Option<String>,
}

impl ParticipantCheckpoint {
    /// Create a bounded checkpoint observation.
    pub fn new(
        participant_id: ParticipantId,
        state: ParticipantCheckpointState,
        committed_revision: Option<u64>,
        diagnostic_code: Option<String>,
    ) -> Result<Self, ParticipantValidationError> {
        let checkpoint = Self {
            participant_id,
            state,
            committed_revision,
            diagnostic_code,
        };
        checkpoint.validate()?;
        Ok(checkpoint)
    }

    /// Validate the retained checkpoint fields without advancing a phase.
    pub fn validate(&self) -> Result<(), ParticipantValidationError> {
        if let Some(diagnostic_code) = &self.diagnostic_code {
            validate_text(
                diagnostic_code,
                ParticipantTextField::DiagnosticCode,
                MAX_DIAGNOSTIC_CODE_BYTES,
            )?;
        }
        Ok(())
    }

    /// Return the participant ID associated with this checkpoint.
    pub const fn participant_id(&self) -> ParticipantId {
        self.participant_id
    }

    /// Return the observed checkpoint state.
    pub const fn state(&self) -> ParticipantCheckpointState {
        self.state
    }

    /// Return the optional owner-reported committed revision.
    pub const fn committed_revision(&self) -> Option<u64> {
        self.committed_revision
    }

    /// Return the optional bounded stable diagnostic code.
    pub fn diagnostic_code(&self) -> Option<&str> {
        self.diagnostic_code.as_deref()
    }
}

/// Bounded set of participant descriptors and their stored checkpoints.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ParticipantSet {
    descriptors: Vec<ParticipantDescriptor>,
    checkpoints: Vec<ParticipantCheckpoint>,
}

impl ParticipantSet {
    /// Create a participant set for a new universal operation.
    pub fn new(
        descriptors: Vec<ParticipantDescriptor>,
        checkpoints: Vec<ParticipantCheckpoint>,
    ) -> Result<Self, ParticipantValidationError> {
        let set = Self {
            descriptors,
            checkpoints,
        };
        set.validate_for_new_operation()?;
        Ok(set)
    }

    /// Validate bounded structure and cross-references, allowing an empty observation for
    /// callers that are inspecting legacy or partially decoded data.
    pub fn validate(&self) -> Result<(), ParticipantValidationError> {
        if self.descriptors.len() > MAX_PARTICIPANTS {
            return Err(ParticipantValidationError::TooManyDescriptors {
                actual: self.descriptors.len(),
                maximum: MAX_PARTICIPANTS,
            });
        }
        if self.checkpoints.len() > MAX_PARTICIPANTS {
            return Err(ParticipantValidationError::TooManyCheckpoints {
                actual: self.checkpoints.len(),
                maximum: MAX_PARTICIPANTS,
            });
        }

        for (index, descriptor) in self.descriptors.iter().enumerate() {
            descriptor.validate()?;
            if self.descriptors[..index]
                .iter()
                .any(|prior| prior.id() == descriptor.id())
            {
                return Err(ParticipantValidationError::DuplicateDescriptorId {
                    participant_id: descriptor.id(),
                });
            }
        }
        for (index, checkpoint) in self.checkpoints.iter().enumerate() {
            checkpoint.validate()?;
            if self.checkpoints[..index]
                .iter()
                .any(|prior| prior.participant_id() == checkpoint.participant_id())
            {
                return Err(ParticipantValidationError::DuplicateCheckpointId {
                    participant_id: checkpoint.participant_id(),
                });
            }
        }

        for descriptor in &self.descriptors {
            let Some(checkpoint) = self
                .checkpoints
                .iter()
                .find(|checkpoint| checkpoint.participant_id() == descriptor.id())
            else {
                return Err(ParticipantValidationError::MissingCheckpoint {
                    participant_id: descriptor.id(),
                });
            };
            if checkpoint.state() == ParticipantCheckpointState::Deferred
                && (descriptor.kind() != ParticipantKind::Artifact
                    || descriptor.requirement() != ParticipantRequirement::OptionalRebuildable)
            {
                return Err(
                    ParticipantValidationError::DeferredCheckpointNotOptionalArtifact {
                        participant_id: descriptor.id(),
                    },
                );
            }
        }
        for checkpoint in &self.checkpoints {
            if !self
                .descriptors
                .iter()
                .any(|descriptor| descriptor.id() == checkpoint.participant_id())
            {
                return Err(ParticipantValidationError::UnknownCheckpoint {
                    participant_id: checkpoint.participant_id(),
                });
            }
        }
        Ok(())
    }

    /// Validate that the set is suitable for a newly admitted universal operation.
    pub fn validate_for_new_operation(&self) -> Result<(), ParticipantValidationError> {
        self.validate()?;
        if self.descriptors.is_empty() {
            return Err(ParticipantValidationError::EmptyForNewOperation);
        }
        Ok(())
    }

    /// Validate the initial checkpoint shape for a newly durable intent.
    ///
    /// Initial intent records describe work that has not started. Every participant therefore
    /// has a `Pending` checkpoint without a committed revision or diagnostic. This validates
    /// stored values only; it does not acquire a participant lease or authorize a transition.
    pub fn validate_for_new_intent(&self) -> Result<(), ParticipantValidationError> {
        self.validate_for_new_operation()?;
        for checkpoint in &self.checkpoints {
            if checkpoint.state() != ParticipantCheckpointState::Pending {
                return Err(ParticipantValidationError::InitialCheckpointNotPending {
                    participant_id: checkpoint.participant_id(),
                    state: checkpoint.state(),
                });
            }
            if checkpoint.committed_revision().is_some() {
                return Err(
                    ParticipantValidationError::InitialCheckpointRevisionForbidden {
                        participant_id: checkpoint.participant_id(),
                    },
                );
            }
            if checkpoint.diagnostic_code().is_some() {
                return Err(
                    ParticipantValidationError::InitialCheckpointDiagnosticForbidden {
                        participant_id: checkpoint.participant_id(),
                    },
                );
            }
        }
        Ok(())
    }

    /// Borrow all stored participant descriptors.
    pub fn descriptors(&self) -> &[ParticipantDescriptor] {
        &self.descriptors
    }

    /// Borrow all stored participant checkpoints.
    pub fn checkpoints(&self) -> &[ParticipantCheckpoint] {
        &self.checkpoints
    }

    /// Find the stored checkpoint for one participant ID.
    pub fn checkpoint_for(&self, participant_id: ParticipantId) -> Option<&ParticipantCheckpoint> {
        self.checkpoints
            .iter()
            .find(|checkpoint| checkpoint.participant_id() == participant_id)
    }

    /// Return whether every required participant is Applied or NotApplicable.
    ///
    /// Callers must also check [`Self::has_audit_observations`] before treating the operation as
    /// successful and account for optional work that is still pending. This method reports
    /// required completion only and does not grant phase authority. Invalid decoded sets never
    /// report required completion.
    pub fn required_complete(&self) -> bool {
        self.validate_for_new_operation().is_ok()
            && self
                .descriptors
                .iter()
                .filter(|descriptor| descriptor.requirement() == ParticipantRequirement::Required)
                .all(|descriptor| {
                    matches!(
                        self.checkpoint_for(descriptor.id())
                            .map(ParticipantCheckpoint::state),
                        Some(ParticipantCheckpointState::Applied)
                            | Some(ParticipantCheckpointState::NotApplicable)
                    )
                })
    }

    /// Return whether an optional rebuildable artifact is recorded as deferred.
    pub fn has_deferred_artifacts(&self) -> bool {
        self.descriptors.iter().any(|descriptor| {
            descriptor.kind() == ParticipantKind::Artifact
                && descriptor.requirement() == ParticipantRequirement::OptionalRebuildable
                && matches!(
                    self.checkpoint_for(descriptor.id())
                        .map(ParticipantCheckpoint::state),
                    Some(ParticipantCheckpointState::Deferred)
                )
        })
    }

    /// Return whether any participant checkpoint carries an audit-required observation.
    pub fn has_audit_observations(&self) -> bool {
        self.checkpoints
            .iter()
            .any(|checkpoint| checkpoint.state() == ParticipantCheckpointState::AuditRequired)
    }
}

/// Field containing a bounded participant string.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParticipantTextField {
    /// Stored source identifier.
    SourceId,
    /// Stored source-root identity.
    RootIdentity,
    /// Stored source-database identity.
    DatabaseIdentity,
    /// Stored opaque artifact key or reference.
    ArtifactKey,
    /// Stored stable diagnostic code.
    DiagnosticCode,
}

impl fmt::Display for ParticipantTextField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::SourceId => "source ID",
            Self::RootIdentity => "root identity",
            Self::DatabaseIdentity => "database identity",
            Self::ArtifactKey => "artifact key",
            Self::DiagnosticCode => "diagnostic code",
        })
    }
}

/// Reason a bounded participant string failed validation.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ParticipantTextViolation {
    /// The string was empty where a value is required.
    #[error("is empty")]
    Empty,
    /// The string exceeded its field-specific byte bound.
    #[error("exceeds its byte bound")]
    TooLong,
    /// The string contains a NUL or another control character.
    #[error("contains a control character")]
    ContainsControl,
}

/// Typed validation failure for untrusted participant observations.
#[derive(Clone, Copy, Debug, Error, PartialEq)]
pub enum ParticipantValidationError {
    /// The descriptor list exceeds [`MAX_PARTICIPANTS`].
    #[error("participant descriptor count {actual} exceeds maximum {maximum}")]
    TooManyDescriptors {
        /// Observed descriptor count.
        actual: usize,
        /// Maximum descriptor count.
        maximum: usize,
    },
    /// The checkpoint list exceeds [`MAX_PARTICIPANTS`].
    #[error("participant checkpoint count {actual} exceeds maximum {maximum}")]
    TooManyCheckpoints {
        /// Observed checkpoint count.
        actual: usize,
        /// Maximum checkpoint count.
        maximum: usize,
    },
    /// A newly admitted universal operation has no participants.
    #[error("a new universal operation must contain at least one participant")]
    EmptyForNewOperation,
    /// An initial intent checkpoint must still be pending.
    #[error(
        "initial participant checkpoint {participant_id} has state {state:?}, expected Pending"
    )]
    InitialCheckpointNotPending {
        /// Participant ID carrying the non-pending state.
        participant_id: ParticipantId,
        /// Stored state that is not valid for initial intent.
        state: ParticipantCheckpointState,
    },
    /// Initial intent checkpoints cannot claim a committed revision.
    #[error("initial participant checkpoint {participant_id} cannot carry a committed revision")]
    InitialCheckpointRevisionForbidden {
        /// Participant ID carrying the revision.
        participant_id: ParticipantId,
    },
    /// Initial intent checkpoints cannot carry a diagnostic code.
    #[error("initial participant checkpoint {participant_id} cannot carry a diagnostic code")]
    InitialCheckpointDiagnosticForbidden {
        /// Participant ID carrying the diagnostic.
        participant_id: ParticipantId,
    },
    /// Two descriptors use the same participant ID.
    #[error("duplicate participant descriptor ID {participant_id}")]
    DuplicateDescriptorId {
        /// Duplicated participant ID.
        participant_id: ParticipantId,
    },
    /// Two checkpoints use the same participant ID.
    #[error("duplicate participant checkpoint ID {participant_id}")]
    DuplicateCheckpointId {
        /// Duplicated participant ID.
        participant_id: ParticipantId,
    },
    /// A descriptor has no checkpoint.
    #[error("participant descriptor {participant_id} has no checkpoint")]
    MissingCheckpoint {
        /// Participant ID missing a checkpoint.
        participant_id: ParticipantId,
    },
    /// A checkpoint does not name a descriptor in the same set.
    #[error("participant checkpoint {participant_id} has no descriptor")]
    UnknownCheckpoint {
        /// Participant ID missing its descriptor.
        participant_id: ParticipantId,
    },
    /// A stored identity, key, or diagnostic code violated its bound or character policy.
    #[error("invalid {field}: {violation}")]
    InvalidText {
        /// Field being validated.
        field: ParticipantTextField,
        /// Bounded-string violation.
        violation: ParticipantTextViolation,
    },
    /// A source fence must carry a positive lease epoch.
    #[error("source fence lease epoch must be positive")]
    InvalidLeaseEpoch,
    /// This participant kind requires a source fence observation.
    #[error("participant kind {kind:?} requires a source fence")]
    SourceFenceRequired {
        /// Participant kind missing its fence.
        kind: ParticipantKind,
    },
    /// A global-library participant cannot bind itself to one source.
    #[error("global-library participant cannot carry a source fence")]
    GlobalLibrarySourceFenceForbidden,
    /// Optional rebuildable work is valid only for artifact participants.
    #[error("optional rebuildable requirement is valid only for artifact participants")]
    OptionalRequirementOnlyArtifact,
    /// An artifact participant must identify its opaque rebuildable key or reference.
    #[error("artifact participant requires an artifact key or reference")]
    ArtifactKeyRequired,
    /// Non-artifact participants cannot carry an artifact key or reference.
    #[error("participant kind {kind:?} cannot carry an artifact key or reference")]
    ArtifactKeyForbidden {
        /// Participant kind carrying the forbidden key.
        kind: ParticipantKind,
    },
    /// Deferred state is valid only for an optional rebuildable artifact.
    #[error("participant {participant_id} is deferred but is not an optional rebuildable artifact")]
    DeferredCheckpointNotOptionalArtifact {
        /// Participant ID carrying the invalid deferred state.
        participant_id: ParticipantId,
    },
}

fn validate_text(
    value: &str,
    field: ParticipantTextField,
    maximum: usize,
) -> Result<(), ParticipantValidationError> {
    if value.is_empty() {
        return Err(ParticipantValidationError::InvalidText {
            field,
            violation: ParticipantTextViolation::Empty,
        });
    }
    if value.len() > maximum {
        return Err(ParticipantValidationError::InvalidText {
            field,
            violation: ParticipantTextViolation::TooLong,
        });
    }
    if value.chars().any(char::is_control) {
        return Err(ParticipantValidationError::InvalidText {
            field,
            violation: ParticipantTextViolation::ContainsControl,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn id(value: &str) -> ParticipantId {
        ParticipantId::parse(value).expect("fixed participant ID is valid")
    }

    fn source_fence() -> StoredSourceFence {
        StoredSourceFence::new(
            SourceId::from_string("source-a"),
            "root-identity",
            "database-identity",
            0,
            0,
            7,
        )
        .expect("source fence fixture is valid")
    }

    fn descriptor(
        participant_id: ParticipantId,
        kind: ParticipantKind,
        requirement: ParticipantRequirement,
        source_fence: Option<StoredSourceFence>,
        artifact_key: Option<&str>,
    ) -> ParticipantDescriptor {
        ParticipantDescriptor::new(
            participant_id,
            kind,
            requirement,
            source_fence,
            artifact_key.map(str::to_owned),
        )
        .expect("descriptor fixture is valid")
    }

    fn checkpoint(
        participant_id: ParticipantId,
        state: ParticipantCheckpointState,
    ) -> ParticipantCheckpoint {
        ParticipantCheckpoint::new(participant_id, state, None, None)
            .expect("checkpoint fixture is valid")
    }

    fn descriptor_json(participant_id: ParticipantId) -> Value {
        json!({
            "id": participant_id,
            "kind": "Filesystem",
            "requirement": "Required",
            "source_fence": null,
            "artifact_key": null,
        })
    }

    fn checkpoint_json(participant_id: ParticipantId) -> Value {
        json!({
            "participant_id": participant_id,
            "state": "Pending",
            "committed_revision": null,
            "diagnostic_code": null,
        })
    }

    #[test]
    fn participant_id_uses_fixed_uuid_string_wire_shape() {
        let participant_id = id("01234567-89ab-cdef-0123-456789abcdef");
        let encoded = serde_json::to_vec(&participant_id).expect("participant ID serializes");
        assert_eq!(encoded, br#""01234567-89ab-cdef-0123-456789abcdef""#);
        let decoded: ParticipantId =
            serde_json::from_slice(&encoded).expect("participant ID decodes");
        assert_eq!(decoded, participant_id);
        assert_eq!(
            participant_id.to_string(),
            "01234567-89ab-cdef-0123-456789abcdef"
        );
    }

    #[test]
    fn invalid_decoded_inventory_cannot_report_required_completion() {
        let participant_id = ParticipantId::new();
        let descriptor = descriptor_json(participant_id);
        let mut applied = checkpoint_json(participant_id);
        applied["state"] = json!("Applied");
        let set: ParticipantSet = serde_json::from_value(json!({
            "descriptors": [descriptor.clone(), descriptor],
            "checkpoints": [applied]
        }))
        .expect("untrusted duplicate inventory decodes");
        assert!(set.validate().is_err());
        assert!(!set.required_complete());
    }

    #[test]
    fn source_fence_requires_positive_epoch_and_accepts_zero_revisions() {
        let fence = source_fence();
        assert_eq!(fence.lifecycle_generation(), 0);
        assert_eq!(fence.source_revision(), 0);
        assert_eq!(fence.lease_epoch(), 7);
        assert!(matches!(
            StoredSourceFence::new(
                SourceId::from_string("source-a"),
                "root",
                "database",
                1,
                2,
                0,
            ),
            Err(ParticipantValidationError::InvalidLeaseEpoch)
        ));
    }

    #[test]
    fn descriptor_kind_rules_reject_wrong_evidence_fields() {
        let participant_id = id("01234567-89ab-cdef-0123-456789abcdef");
        assert!(matches!(
            ParticipantDescriptor::new(
                participant_id,
                ParticipantKind::SourceDatabase,
                ParticipantRequirement::Required,
                None,
                None,
            ),
            Err(ParticipantValidationError::SourceFenceRequired {
                kind: ParticipantKind::SourceDatabase
            })
        ));
        assert!(matches!(
            ParticipantDescriptor::new(
                participant_id,
                ParticipantKind::GlobalLibrary,
                ParticipantRequirement::Required,
                Some(source_fence()),
                None,
            ),
            Err(ParticipantValidationError::GlobalLibrarySourceFenceForbidden)
        ));
        assert!(matches!(
            ParticipantDescriptor::new(
                participant_id,
                ParticipantKind::Filesystem,
                ParticipantRequirement::Required,
                None,
                Some(String::from("artifact")),
            ),
            Err(ParticipantValidationError::ArtifactKeyForbidden {
                kind: ParticipantKind::Filesystem
            })
        ));
        assert!(matches!(
            ParticipantDescriptor::new(
                participant_id,
                ParticipantKind::Artifact,
                ParticipantRequirement::Required,
                Some(source_fence()),
                None,
            ),
            Err(ParticipantValidationError::ArtifactKeyRequired)
        ));
        assert!(matches!(
            ParticipantDescriptor::new(
                participant_id,
                ParticipantKind::Harvest,
                ParticipantRequirement::OptionalRebuildable,
                None,
                None,
            ),
            Err(ParticipantValidationError::OptionalRequirementOnlyArtifact)
        ));
    }

    #[test]
    fn participant_set_requires_one_checkpoint_per_unique_descriptor() {
        let first = id("01234567-89ab-cdef-0123-456789abcdef");
        let second = id("11234567-89ab-cdef-0123-456789abcdef");
        let duplicate_descriptors = serde_json::from_value::<ParticipantSet>(json!({
            "descriptors": [descriptor_json(first), descriptor_json(first)],
            "checkpoints": [checkpoint_json(first)],
        }))
        .expect("duplicate fixture deserializes as untrusted data");
        assert!(matches!(
            duplicate_descriptors.validate(),
            Err(ParticipantValidationError::DuplicateDescriptorId { participant_id })
                if participant_id == first
        ));

        let missing_checkpoint = serde_json::from_value::<ParticipantSet>(json!({
            "descriptors": [descriptor_json(first)],
            "checkpoints": [],
        }))
        .expect("missing fixture deserializes as untrusted data");
        assert!(matches!(
            missing_checkpoint.validate(),
            Err(ParticipantValidationError::MissingCheckpoint { participant_id })
                if participant_id == first
        ));

        let unknown_checkpoint = serde_json::from_value::<ParticipantSet>(json!({
            "descriptors": [descriptor_json(first)],
            "checkpoints": [checkpoint_json(second)],
        }))
        .expect("unknown fixture deserializes as untrusted data");
        assert!(matches!(
            unknown_checkpoint.validate(),
            Err(ParticipantValidationError::MissingCheckpoint { participant_id })
                if participant_id == first
        ));

        let duplicate_checkpoints = serde_json::from_value::<ParticipantSet>(json!({
            "descriptors": [descriptor_json(first)],
            "checkpoints": [checkpoint_json(first), checkpoint_json(first)],
        }))
        .expect("duplicate checkpoint fixture deserializes as untrusted data");
        assert!(matches!(
            duplicate_checkpoints.validate(),
            Err(ParticipantValidationError::DuplicateCheckpointId { participant_id })
                if participant_id == first
        ));
    }

    #[test]
    fn deferred_and_audit_states_are_classified_without_phase_authority() {
        let required_id = id("01234567-89ab-cdef-0123-456789abcdef");
        let artifact_id = id("11234567-89ab-cdef-0123-456789abcdef");
        let set = ParticipantSet::new(
            vec![
                descriptor(
                    required_id,
                    ParticipantKind::SourceDatabase,
                    ParticipantRequirement::Required,
                    Some(source_fence()),
                    None,
                ),
                descriptor(
                    artifact_id,
                    ParticipantKind::Artifact,
                    ParticipantRequirement::OptionalRebuildable,
                    Some(source_fence()),
                    Some("waveform-v1"),
                ),
            ],
            vec![
                checkpoint(required_id, ParticipantCheckpointState::Applied),
                checkpoint(artifact_id, ParticipantCheckpointState::Deferred),
            ],
        )
        .expect("deferred optional artifact set is valid");
        assert!(set.required_complete());
        assert!(set.has_deferred_artifacts());
        assert!(!set.has_audit_observations());

        let audit_set = ParticipantSet::new(
            vec![
                descriptor(
                    required_id,
                    ParticipantKind::SourceDatabase,
                    ParticipantRequirement::Required,
                    Some(source_fence()),
                    None,
                ),
                descriptor(
                    artifact_id,
                    ParticipantKind::Artifact,
                    ParticipantRequirement::OptionalRebuildable,
                    Some(source_fence()),
                    Some("waveform-v1"),
                ),
            ],
            vec![
                checkpoint(required_id, ParticipantCheckpointState::Applied),
                checkpoint(artifact_id, ParticipantCheckpointState::AuditRequired),
            ],
        )
        .expect("audit observation set is valid");
        assert!(audit_set.required_complete());
        assert!(audit_set.has_audit_observations());
        assert!(!audit_set.has_deferred_artifacts());
    }

    #[test]
    fn required_deferred_checkpoint_is_rejected() {
        let participant_id = id("01234567-89ab-cdef-0123-456789abcdef");
        let set = serde_json::from_value::<ParticipantSet>(json!({
            "descriptors": [descriptor_json(participant_id)],
            "checkpoints": [{
                "participant_id": participant_id,
                "state": "Deferred",
                "committed_revision": null,
                "diagnostic_code": null,
            }],
        }))
        .expect("deferred fixture deserializes as untrusted data");
        assert!(matches!(
            set.validate(),
            Err(ParticipantValidationError::DeferredCheckpointNotOptionalArtifact {
                participant_id: actual
            }) if actual == participant_id
        ));
    }

    #[test]
    fn empty_set_is_invalid_only_for_new_operations() {
        let empty = serde_json::from_value::<ParticipantSet>(json!({
            "descriptors": [],
            "checkpoints": [],
        }))
        .expect("empty observation deserializes");
        assert!(empty.validate().is_ok());
        assert!(matches!(
            empty.validate_for_new_operation(),
            Err(ParticipantValidationError::EmptyForNewOperation)
        ));
        assert!(!empty.required_complete());
    }

    #[test]
    fn all_bounded_strings_reject_limits_and_controls() {
        assert!(matches!(
            StoredSourceFence::new(
                SourceId::from_string("s".repeat(MAX_SOURCE_IDENTITY_BYTES + 1)),
                "root",
                "database",
                0,
                0,
                1,
            ),
            Err(ParticipantValidationError::InvalidText {
                field: ParticipantTextField::SourceId,
                violation: ParticipantTextViolation::TooLong,
            })
        ));
        assert!(matches!(
            StoredSourceFence::new(
                SourceId::from_string("source"),
                "root\nidentity",
                "database",
                0,
                0,
                1,
            ),
            Err(ParticipantValidationError::InvalidText {
                field: ParticipantTextField::RootIdentity,
                violation: ParticipantTextViolation::ContainsControl,
            })
        ));

        let participant_id = id("01234567-89ab-cdef-0123-456789abcdef");
        assert!(matches!(
            ParticipantDescriptor::new(
                participant_id,
                ParticipantKind::Artifact,
                ParticipantRequirement::Required,
                Some(source_fence()),
                Some("k".repeat(MAX_ARTIFACT_REFERENCE_BYTES + 1)),
            ),
            Err(ParticipantValidationError::InvalidText {
                field: ParticipantTextField::ArtifactKey,
                violation: ParticipantTextViolation::TooLong,
            })
        ));
        assert!(matches!(
            ParticipantCheckpoint::new(
                participant_id,
                ParticipantCheckpointState::AuditRequired,
                None,
                Some("code\0with-nul".to_owned()),
            ),
            Err(ParticipantValidationError::InvalidText {
                field: ParticipantTextField::DiagnosticCode,
                violation: ParticipantTextViolation::ContainsControl,
            })
        ));
    }

    #[test]
    fn participant_sets_are_strictly_serialized() {
        let participant_id = id("01234567-89ab-cdef-0123-456789abcdef");
        let mut descriptor = descriptor_json(participant_id);
        descriptor["unexpected"] = json!(true);
        assert!(serde_json::from_value::<ParticipantDescriptor>(descriptor).is_err());

        let mut fence = json!({
            "source_id": "source",
            "root_identity": "root",
            "database_identity": "database",
            "lifecycle_generation": 0,
            "source_revision": 0,
            "lease_epoch": 1,
            "unexpected": false,
        });
        assert!(serde_json::from_value::<StoredSourceFence>(fence.take()).is_err());

        let mut checkpoint = checkpoint_json(participant_id);
        checkpoint["unexpected"] = json!("field");
        assert!(serde_json::from_value::<ParticipantCheckpoint>(checkpoint).is_err());

        let mut set = json!({
            "descriptors": [descriptor_json(participant_id)],
            "checkpoints": [checkpoint_json(participant_id)],
            "unexpected": [],
        });
        assert!(serde_json::from_value::<ParticipantSet>(set.take()).is_err());
    }

    #[test]
    fn participant_count_bounds_are_checked_before_cross_references() {
        let mut descriptors = Vec::new();
        let mut checkpoints = Vec::new();
        for index in 0..=MAX_PARTICIPANTS {
            let participant_id = ParticipantId::from_uuid(Uuid::from_u128(index as u128 + 1));
            descriptors.push(descriptor(
                participant_id,
                ParticipantKind::Filesystem,
                ParticipantRequirement::Required,
                None,
                None,
            ));
            checkpoints.push(checkpoint(
                participant_id,
                ParticipantCheckpointState::Pending,
            ));
        }
        assert!(matches!(
            ParticipantSet::new(descriptors, checkpoints),
            Err(ParticipantValidationError::TooManyDescriptors { actual, maximum })
                if actual == MAX_PARTICIPANTS + 1 && maximum == MAX_PARTICIPANTS
        ));
    }
}
