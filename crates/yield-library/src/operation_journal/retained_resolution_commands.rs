//! Bounded observations for explicit retained-folder restore and purge requests.
//!
//! These values describe an already-retained legacy folder-delete entry. They do not import the
//! legacy journal, inspect the filesystem, capture metadata, or authorize a restore or purge. The
//! selected-entry fingerprint is calculated from caller-supplied bytes only; a future owner must
//! isolate the complete selected legacy entry (including restore metadata), re-read it, and
//! compare the bytes before preparing any action. The digest is an observation and cannot establish
//! provenance or provide execution authority by itself.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

use super::coordinator::{StoredRelativePath, StoredRelativePathError};
use super::model::UniversalCommandKind;
use super::namespace_commands::{
    NamespaceCommandValidationError, NamespaceEndpointRole, NamespaceEntryEndpoint,
    NamespaceEntryKind, validate_persisted_namespace_entry_endpoint_value,
};
use super::participants::{
    MAX_SOURCE_IDENTITY_BYTES, ParticipantDescriptor, ParticipantKind, ParticipantRequirement,
    ParticipantSet, ParticipantValidationError,
};

/// The fixed source-relative directory used by the legacy retained-delete journal.
pub const RETAINED_RECOVERY_DIRECTORY: &str = ".yield_delete_staging";
/// Maximum bytes accepted for one isolated selected legacy journal-entry JSON value.
///
/// This is intentionally a per-call hashing bound separate from [`super::model::MAX_RECORD_BYTES`].
/// The caller must isolate one complete entry before calling the helper; oversized entries are
/// rejected and preserved for a future bounded streaming capture path rather than truncated.
pub const MAX_RETAINED_SELECTED_ENTRY_JSON_BYTES: usize = 64 * 1024;
const RETAINED_ENTRY_FINGERPRINT_DOMAIN: &[u8] = b"yield-retained-delete-selected-entry-v1\0";

/// UUID identity used by the legacy retained-folder journal.
///
/// Legacy restaging reuses this identity, so it is retained as an observed row key rather than
/// treated as a unique generation identity. The selected-entry fingerprint is a bounded snapshot
/// observation that can detect changed bytes; it is not a unique generation token because a
/// later restage may reproduce the same complete entry bytes. A future owner must recapture the
/// entry and its staged source revision and physical identity before preparing an action.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct RetainedDeleteId(Uuid);

impl RetainedDeleteId {
    /// Construct a non-nil retained-delete identity.
    pub fn new(value: Uuid) -> Result<Self, RetainedResolutionValidationError> {
        let id = Self(value);
        id.validate()?;
        Ok(id)
    }

    /// Wrap an existing UUID without treating it as a live capability.
    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    /// Return the UUID spelling used by the persisted representation.
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }

    /// Validate an identity recovered from untrusted storage.
    pub fn validate(self) -> Result<(), RetainedResolutionValidationError> {
        if self.0.is_nil() {
            Err(RetainedResolutionValidationError::InvalidRetainedDeleteId)
        } else {
            Ok(())
        }
    }
}

/// Fixed digest of one complete selected legacy journal-entry JSON value.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct RetainedEntryFingerprint([u8; 32]);

impl RetainedEntryFingerprint {
    /// Wrap a fixed-size digest returned by the fingerprint helper.
    pub const fn from_bytes(value: [u8; 32]) -> Self {
        Self(value)
    }

    /// Borrow the fixed-size digest bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Failure while calculating a bounded selected-entry fingerprint.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum RetainedFingerprintValidationError {
    /// The selected-entry JSON input was empty.
    #[error("retained selected-entry JSON is empty")]
    Empty,
    /// The selected-entry JSON input exceeded the bounded helper limit.
    #[error("retained selected-entry JSON exceeds {maximum} bytes: {actual} bytes")]
    TooLong {
        /// Actual input byte count.
        actual: usize,
        /// Maximum accepted input byte count.
        maximum: usize,
    },
}

/// Fingerprint complete selected-entry JSON bytes with a versioned domain separator.
///
/// The caller must supply the exact bytes for one isolated selected legacy entry, including all
/// fields that affect recovery such as `restore_stamp`, and must derive the retained observation's
/// extracted fields from that same capture. A future owner must re-read and compare the complete
/// entry before entering its prepared phase. This pure helper performs no JSON parsing or I/O;
/// its digest is an observation and cannot prove provenance or authorize an action.
pub fn fingerprint_selected_entry_json(
    bytes: &[u8],
) -> Result<RetainedEntryFingerprint, RetainedFingerprintValidationError> {
    if bytes.is_empty() {
        return Err(RetainedFingerprintValidationError::Empty);
    }
    if bytes.len() > MAX_RETAINED_SELECTED_ENTRY_JSON_BYTES {
        return Err(RetainedFingerprintValidationError::TooLong {
            actual: bytes.len(),
            maximum: MAX_RETAINED_SELECTED_ENTRY_JSON_BYTES,
        });
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(RETAINED_ENTRY_FINGERPRINT_DOMAIN);
    hasher.update(bytes);
    Ok(RetainedEntryFingerprint::from_bytes(
        *hasher.finalize().as_bytes(),
    ))
}

/// The legacy retained-delete stage observed by a new resolution request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum RetainedResolutionStage {
    /// The folder is retained and eligible for one explicit Restore or Purge request.
    Deleted,
    /// The legacy journal is already recovering a restore; it is not a new action target.
    RestorePendingDb,
}

/// The fixed legacy source-local recovery-root identity used by retained-folder entries.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum RetainedRecoveryRootKind {
    /// The legacy `.yield_delete_staging` source-local directory.
    LegacyDeleteStaging,
}

/// User decision recorded by an initial retained-folder purge intent.
///
/// This is a durable record of the user's choice only. It carries no confirmation timestamp,
/// token, live capability, protected-source override, or permission to remove anything.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum RetainedPurgeDecision {
    /// The user explicitly confirmed the permanent purge choice.
    UserConfirmed,
    /// No explicit confirmation was captured, so the initial purge intent is invalid.
    NotConfirmed,
}

/// Fixed conflict policy for a retained-folder restore.
///
/// The later prepared merge must preserve both copies when file contents conflict and may reuse
/// exact matches. The policy is intentionally not serialized into the initial intent because it
/// is a command invariant rather than a caller-selected authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetainedRestoreConflictPolicy {
    /// Preserve conflicting data using the legacy keep-both merge behavior.
    PreserveConflictsKeepBoth,
}

/// Bounded observation of one retained legacy folder-delete entry.
///
/// `original_relative` is only the nonempty source-relative path from the legacy entry. The original
/// directory may be absent or independently recreated, so this intent deliberately carries no
/// fabricated original physical identity. `staged` retains the source-database endpoint that
/// binds the existing retained directory. It is required to use the source-relative locator
/// `.yield_delete_staging/<staged_relative>` and to identify a directory at the fenced source
/// revision. It contains no legacy metadata snapshot, prepared merge plan, restore stamp, or
/// filesystem handle.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedFolderObservation {
    original_relative: StoredRelativePath,
    staged: NamespaceEntryEndpoint,
    staged_relative: StoredRelativePath,
    retained_id: RetainedDeleteId,
    stage: RetainedResolutionStage,
    recovery_root_kind: RetainedRecoveryRootKind,
    selected_entry_fingerprint: RetainedEntryFingerprint,
}

impl RetainedFolderObservation {
    /// Construct one retained-folder observation without reading the legacy journal or filesystem.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        original_relative: StoredRelativePath,
        staged: NamespaceEntryEndpoint,
        staged_relative: StoredRelativePath,
        retained_id: RetainedDeleteId,
        stage: RetainedResolutionStage,
        recovery_root_kind: RetainedRecoveryRootKind,
        selected_entry_fingerprint: RetainedEntryFingerprint,
    ) -> Result<Self, RetainedResolutionValidationError> {
        let observation = Self {
            original_relative,
            staged,
            staged_relative,
            retained_id,
            stage,
            recovery_root_kind,
            selected_entry_fingerprint,
        };
        observation.validate_shape()?;
        Ok(observation)
    }

    /// Return the staged source-local recovery-folder endpoint observation.
    pub fn staged(&self) -> &NamespaceEntryEndpoint {
        &self.staged
    }

    /// Return the original source-relative folder path.
    pub fn original_relative(&self) -> &StoredRelativePath {
        &self.original_relative
    }

    /// Return the validated legacy staged-relative locator.
    pub fn staged_relative(&self) -> &StoredRelativePath {
        &self.staged_relative
    }

    /// Return the observed legacy retained-entry UUID.
    pub const fn retained_id(&self) -> RetainedDeleteId {
        self.retained_id
    }

    /// Return the observed legacy journal stage.
    pub const fn stage(&self) -> RetainedResolutionStage {
        self.stage
    }

    /// Return the fixed recovery-root kind.
    pub const fn recovery_root_kind(&self) -> RetainedRecoveryRootKind {
        self.recovery_root_kind
    }

    /// Return the selected-entry fingerprint observation.
    pub const fn selected_entry_fingerprint(&self) -> RetainedEntryFingerprint {
        self.selected_entry_fingerprint
    }

    /// Validate the observation against the complete participant inventory.
    pub fn validate_against_participants(
        &self,
        participants: &ParticipantSet,
    ) -> Result<(), RetainedResolutionValidationError> {
        self.validate_shape()?;
        participants.validate_for_new_intent()?;

        validate_endpoint_against_participants(participants, &self.staged)?;

        if !participants.descriptors().iter().any(|descriptor| {
            descriptor.kind() == ParticipantKind::Filesystem
                && descriptor.requirement() == ParticipantRequirement::Required
        }) {
            return Err(RetainedResolutionValidationError::RequiredFilesystemParticipantMissing);
        }
        Ok(())
    }

    fn validate_shape(&self) -> Result<(), RetainedResolutionValidationError> {
        validate_endpoint_shape(&self.staged)?;
        if self.staged.expectation().kind() != NamespaceEntryKind::Directory {
            return Err(RetainedResolutionValidationError::StagedMustBeDirectory);
        }
        self.original_relative
            .validate()
            .map_err(RetainedResolutionValidationError::InvalidOriginalPath)?;
        if is_recovery_namespace_path(self.original_relative.as_str()) {
            return Err(RetainedResolutionValidationError::RecoveryNamespaceOverlap);
        }
        self.retained_id.validate()?;
        if self.stage != RetainedResolutionStage::Deleted {
            return Err(RetainedResolutionValidationError::IneligibleStage { actual: self.stage });
        }
        if self.recovery_root_kind != RetainedRecoveryRootKind::LegacyDeleteStaging {
            return Err(RetainedResolutionValidationError::UnsupportedRecoveryRootKind);
        }
        self.staged_relative
            .validate()
            .map_err(RetainedResolutionValidationError::InvalidStagedRelativePath)?;
        if is_recovery_namespace_path(self.staged_relative.as_str()) {
            return Err(RetainedResolutionValidationError::RecoveryNamespaceOverlap);
        }
        let expected_staged = compose_staged_path(&self.staged_relative)?;
        if self.staged.relative_path() != &expected_staged {
            return Err(RetainedResolutionValidationError::StagedPathMismatch);
        }
        Ok(())
    }
}

/// Initial restore request for one retained folder.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedRestoreIntentArgs {
    observation: RetainedFolderObservation,
}

impl RetainedRestoreIntentArgs {
    /// Construct an initial restore intent without preparing a merge or touching the filesystem.
    pub fn new(
        observation: RetainedFolderObservation,
    ) -> Result<Self, RetainedResolutionValidationError> {
        observation.validate_shape()?;
        Ok(Self { observation })
    }

    /// Return the retained-folder observation.
    pub fn observation(&self) -> &RetainedFolderObservation {
        &self.observation
    }

    /// Return the fixed conflict policy for a later prepared restore merge.
    pub const fn collision_policy(&self) -> RetainedRestoreConflictPolicy {
        RetainedRestoreConflictPolicy::PreserveConflictsKeepBoth
    }

    /// Validate the restore observation against the complete participant inventory.
    pub fn validate_against_participants(
        &self,
        participants: &ParticipantSet,
    ) -> Result<(), RetainedResolutionValidationError> {
        self.observation.validate_against_participants(participants)
    }
}

/// Initial purge request for one retained folder.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedPurgeIntentArgs {
    observation: RetainedFolderObservation,
    decision: RetainedPurgeDecision,
}

impl RetainedPurgeIntentArgs {
    /// Construct an initial explicitly confirmed purge intent without deleting anything.
    pub fn new(
        observation: RetainedFolderObservation,
        decision: RetainedPurgeDecision,
    ) -> Result<Self, RetainedResolutionValidationError> {
        observation.validate_shape()?;
        validate_purge_decision(decision)?;
        Ok(Self {
            observation,
            decision,
        })
    }

    /// Return the retained-folder observation.
    pub fn observation(&self) -> &RetainedFolderObservation {
        &self.observation
    }

    /// Return the captured user decision.
    pub const fn decision(&self) -> RetainedPurgeDecision {
        self.decision
    }

    /// Validate the purge observation against the complete participant inventory.
    pub fn validate_against_participants(
        &self,
        participants: &ParticipantSet,
    ) -> Result<(), RetainedResolutionValidationError> {
        self.observation
            .validate_against_participants(participants)?;
        validate_purge_decision(self.decision)
    }
}

/// Retained-folder restore or purge command arguments carried by schema V11.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum RetainedResolutionCommandArgs {
    /// Restore a retained folder using the later fixed preserve-conflicts merge policy.
    Restore(RetainedRestoreIntentArgs),
    /// Purge a retained folder after an explicit user-confirmed decision.
    Purge(RetainedPurgeIntentArgs),
}

impl RetainedResolutionCommandArgs {
    /// Return the explicit V11 command kind represented by this request.
    pub const fn command_kind(&self) -> UniversalCommandKind {
        match self {
            Self::Restore(_) => UniversalCommandKind::RestoreRetained,
            Self::Purge(_) => UniversalCommandKind::PurgeRetained,
        }
    }

    /// Validate command observations against the complete participant inventory.
    pub fn validate_against_participants(
        &self,
        participants: &ParticipantSet,
    ) -> Result<(), RetainedResolutionValidationError> {
        match self {
            Self::Restore(args) => args.validate_against_participants(participants),
            Self::Purge(args) => args.validate_against_participants(participants),
        }
    }
}

/// Failure while constructing or validating a schema-V11 retained-resolution observation.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum RetainedResolutionValidationError {
    /// The original stored-relative path was malformed.
    #[error("invalid retained original path: {0}")]
    InvalidOriginalPath(#[source] StoredRelativePathError),
    /// The staged endpoint path or staged-relative locator was malformed.
    #[error("invalid retained staged path: {0}")]
    InvalidStagedPath(#[source] StoredRelativePathError),
    /// The staged-relative locator was malformed.
    #[error("invalid retained staged-relative path: {0}")]
    InvalidStagedRelativePath(#[source] StoredRelativePathError),
    /// The staged endpoint shape failed the existing namespace contract.
    #[error("invalid retained staged endpoint: {0}")]
    StagedEndpoint(#[source] NamespaceCommandValidationError),
    /// The staged endpoint was not a directory.
    #[error("retained resolution staged endpoint must be a directory")]
    StagedMustBeDirectory,
    /// The retained ID was nil or otherwise not usable as a legacy UUID identity.
    #[error("retained delete ID is invalid")]
    InvalidRetainedDeleteId,
    /// The observed legacy stage is not eligible for a new action.
    #[error("retained resolution requires Deleted stage, found {actual:?}")]
    IneligibleStage {
        /// Stage observed in the legacy journal.
        actual: RetainedResolutionStage,
    },
    /// The command carried an unsupported recovery-root kind.
    #[error("retained resolution recovery-root kind is unsupported")]
    UnsupportedRecoveryRootKind,
    /// The staged endpoint did not use the exact fixed recovery-root composition.
    #[error("retained resolution staged endpoint does not match its recovery-relative path")]
    StagedPathMismatch,
    /// An original or staged-relative path named the reserved recovery namespace.
    #[error("retained resolution path overlaps the reserved recovery namespace")]
    RecoveryNamespaceOverlap,
    /// Purge was not explicitly confirmed by the user.
    #[error("retained purge requires an explicit user-confirmed decision")]
    PurgeNotConfirmed,
    /// No required Filesystem participant was present.
    #[error("retained resolution requires a required Filesystem participant")]
    RequiredFilesystemParticipantMissing,
    /// The participant inventory failed its complete shape checks.
    #[error("invalid retained resolution participants: {0}")]
    Participants(#[from] ParticipantValidationError),
}

fn validate_endpoint_shape(
    endpoint: &NamespaceEntryEndpoint,
) -> Result<(), RetainedResolutionValidationError> {
    endpoint
        .relative_path()
        .validate()
        .map_err(RetainedResolutionValidationError::InvalidStagedPath)?;
    if endpoint.source_id().as_str().is_empty()
        || endpoint.source_id().as_str().len() > MAX_SOURCE_IDENTITY_BYTES
        || endpoint.source_id().as_str().chars().any(char::is_control)
    {
        return Err(RetainedResolutionValidationError::StagedEndpoint(
            NamespaceCommandValidationError::InvalidSourceIdentity,
        ));
    }
    endpoint
        .expectation()
        .validate()
        .map_err(RetainedResolutionValidationError::StagedEndpoint)
}

fn validate_endpoint_against_participants<'a>(
    participants: &'a ParticipantSet,
    endpoint: &NamespaceEntryEndpoint,
) -> Result<&'a ParticipantDescriptor, RetainedResolutionValidationError> {
    validate_endpoint_shape(endpoint)?;
    let descriptor = participants
        .descriptors()
        .iter()
        .find(|descriptor| descriptor.id() == endpoint.participant_id())
        .ok_or(RetainedResolutionValidationError::StagedEndpoint(
            NamespaceCommandValidationError::ParticipantMissing {
                role: NamespaceEndpointRole::Source,
            },
        ))?;
    if descriptor.kind() != ParticipantKind::SourceDatabase {
        return Err(RetainedResolutionValidationError::StagedEndpoint(
            NamespaceCommandValidationError::ParticipantWrongKind {
                role: NamespaceEndpointRole::Source,
            },
        ));
    }
    if descriptor.requirement() != ParticipantRequirement::Required {
        return Err(RetainedResolutionValidationError::StagedEndpoint(
            NamespaceCommandValidationError::ParticipantNotRequired {
                role: NamespaceEndpointRole::Source,
            },
        ));
    }
    let fence =
        descriptor
            .source_fence()
            .ok_or(RetainedResolutionValidationError::StagedEndpoint(
                NamespaceCommandValidationError::ParticipantFenceMissing {
                    role: NamespaceEndpointRole::Source,
                },
            ))?;
    if fence.source_id() != endpoint.source_id() {
        return Err(RetainedResolutionValidationError::StagedEndpoint(
            NamespaceCommandValidationError::ParticipantSourceMismatch {
                role: NamespaceEndpointRole::Source,
            },
        ));
    }
    if fence.source_revision() != endpoint.expectation().source_revision() {
        return Err(RetainedResolutionValidationError::StagedEndpoint(
            NamespaceCommandValidationError::SourceRevisionMismatch {
                role: NamespaceEndpointRole::Source,
            },
        ));
    }
    Ok(descriptor)
}

fn compose_staged_path(
    staged_relative: &StoredRelativePath,
) -> Result<StoredRelativePath, RetainedResolutionValidationError> {
    StoredRelativePath::new(format!(
        "{RETAINED_RECOVERY_DIRECTORY}/{}",
        staged_relative.as_str()
    ))
    .map_err(RetainedResolutionValidationError::InvalidStagedPath)
}

fn is_recovery_namespace_path(path: &str) -> bool {
    path.split('/')
        .next()
        .is_some_and(|component| component.eq_ignore_ascii_case(RETAINED_RECOVERY_DIRECTORY))
}

fn validate_purge_decision(
    decision: RetainedPurgeDecision,
) -> Result<(), RetainedResolutionValidationError> {
    match decision {
        RetainedPurgeDecision::UserConfirmed => Ok(()),
        RetainedPurgeDecision::NotConfirmed => {
            Err(RetainedResolutionValidationError::PurgeNotConfirmed)
        }
    }
}

/// Validate a tagged V11 retained-resolution value before typed deserialization.
pub(crate) fn validate_persisted_retained_resolution_command_args_value(
    value: &Value,
) -> Result<(), &'static str> {
    let object = strict_object(value, &["Restore", "Purge"])?;
    if object.len() != 1 {
        return Err("persisted retained resolution command must contain exactly one variant");
    }
    let (variant, arguments) = object
        .iter()
        .next()
        .ok_or("persisted retained resolution command must contain exactly one variant")?;
    match variant.as_str() {
        "Restore" => validate_persisted_restore_value(arguments),
        "Purge" => validate_persisted_purge_value(arguments),
        _ => Err("persisted retained resolution command contains an unknown variant"),
    }
}

fn validate_persisted_restore_value(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(value, &["observation"])?;
    validate_persisted_observation_value(required_field(object, "observation")?)
}

fn validate_persisted_purge_value(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(value, &["observation", "decision"])?;
    validate_persisted_observation_value(required_field(object, "observation")?)?;
    if required_field(object, "decision")?.as_str() != Some("UserConfirmed") {
        return Err("retained purge requires UserConfirmed decision");
    }
    Ok(())
}

fn validate_persisted_observation_value(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(
        value,
        &[
            "original_relative",
            "staged",
            "staged_relative",
            "retained_id",
            "stage",
            "recovery_root_kind",
            "selected_entry_fingerprint",
        ],
    )?;
    for field in [
        "original_relative",
        "staged",
        "staged_relative",
        "retained_id",
        "stage",
        "recovery_root_kind",
        "selected_entry_fingerprint",
    ] {
        required_field(object, field)?;
    }
    let original = required_field(object, "original_relative")?
        .as_str()
        .ok_or("retained original path must be a string")?;
    let original =
        StoredRelativePath::new(original).map_err(|_| "retained original path is invalid")?;
    if is_recovery_namespace_path(original.as_str()) {
        return Err("retained original path overlaps recovery namespace");
    }
    validate_persisted_namespace_entry_endpoint_value(required_field(object, "staged")?)?;
    let staged = required_field(object, "staged")?
        .as_object()
        .ok_or("retained staged endpoint must be a JSON object")?;
    let staged_path = required_field(staged, "relative_path")?
        .as_str()
        .ok_or("retained staged endpoint path must be a string")?;
    let staged_path = StoredRelativePath::new(staged_path)
        .map_err(|_| "retained staged endpoint path is invalid")?;
    let staged_relative = required_field(object, "staged_relative")?
        .as_str()
        .ok_or("retained staged-relative path must be a string")?;
    let staged_relative = StoredRelativePath::new(staged_relative)
        .map_err(|_| "retained staged-relative path is invalid")?;
    if is_recovery_namespace_path(staged_relative.as_str()) {
        return Err("retained staged-relative path overlaps recovery namespace");
    }
    let expected_staged = compose_staged_path(&staged_relative)
        .map_err(|_| "retained staged path exceeds its composed bound")?;
    if staged_path != expected_staged {
        return Err("retained staged endpoint does not match staged-relative path");
    }
    let retained_id = required_field(object, "retained_id")?
        .as_str()
        .and_then(|value| Uuid::parse_str(value).ok())
        .ok_or("retained delete ID must be a UUID string")?;
    if retained_id.is_nil() {
        return Err("retained delete ID must not be nil");
    }
    if required_field(object, "stage")?.as_str() != Some("Deleted") {
        return Err("retained resolution requires Deleted stage");
    }
    if required_field(object, "recovery_root_kind")?.as_str() != Some("LegacyDeleteStaging") {
        return Err("retained resolution recovery-root kind is unsupported");
    }
    validate_persisted_fingerprint(required_field(object, "selected_entry_fingerprint")?)
}

fn validate_persisted_fingerprint(value: &Value) -> Result<(), &'static str> {
    let values = value
        .as_array()
        .ok_or("retained selected-entry fingerprint must be a 32-byte array")?;
    if values.len() != 32 {
        return Err("retained selected-entry fingerprint must contain exactly 32 bytes");
    }
    for value in values {
        if value.as_u64().is_none_or(|byte| byte > u64::from(u8::MAX)) {
            return Err("retained selected-entry fingerprint contains an invalid byte");
        }
    }
    Ok(())
}

fn strict_object<'a>(
    value: &'a Value,
    allowed_fields: &[&str],
) -> Result<&'a serde_json::Map<String, Value>, &'static str> {
    let object = value
        .as_object()
        .ok_or("persisted retained resolution value must be a JSON object")?;
    if object
        .keys()
        .any(|field| !allowed_fields.contains(&field.as_str()))
    {
        return Err("persisted retained resolution object contains an unknown field");
    }
    Ok(object)
}

fn required_field<'a>(
    object: &'a serde_json::Map<String, Value>,
    field: &str,
) -> Result<&'a Value, &'static str> {
    object
        .get(field)
        .ok_or("persisted retained resolution object is missing a required field")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operation_journal::{
        NamespaceDirectoryExpectation, NamespaceEntryExpectation, ParticipantCheckpoint,
        ParticipantCheckpointState, StoredSourceFence,
    };
    use crate::sample_sources::SourceId;

    fn fixture() -> (
        ParticipantSet,
        StoredRelativePath,
        NamespaceEntryEndpoint,
        StoredRelativePath,
    ) {
        let source_id = SourceId::from_string("source-a");
        let participant_id =
            super::super::participants::ParticipantId::from_uuid(Uuid::from_u128(1));
        let filesystem_id =
            super::super::participants::ParticipantId::from_uuid(Uuid::from_u128(2));
        let fence = StoredSourceFence::new(source_id.clone(), "source-root", "source-db", 1, 7, 1)
            .expect("source fence");
        let participants = ParticipantSet::new(
            vec![
                ParticipantDescriptor::new(
                    participant_id,
                    ParticipantKind::SourceDatabase,
                    ParticipantRequirement::Required,
                    Some(fence),
                    None,
                )
                .expect("source descriptor"),
                ParticipantDescriptor::new(
                    filesystem_id,
                    ParticipantKind::Filesystem,
                    ParticipantRequirement::Required,
                    None,
                    None,
                )
                .expect("filesystem descriptor"),
            ],
            vec![
                ParticipantCheckpoint::new(
                    participant_id,
                    ParticipantCheckpointState::Pending,
                    None,
                    None,
                )
                .expect("source checkpoint"),
                ParticipantCheckpoint::new(
                    filesystem_id,
                    ParticipantCheckpointState::Pending,
                    None,
                    None,
                )
                .expect("filesystem checkpoint"),
            ],
        )
        .expect("participants");
        let original_path = StoredRelativePath::new("drums/kick").expect("original path");
        let staged_relative = StoredRelativePath::new("drums/kick").expect("staged relative");
        let staged_path = compose_staged_path(&staged_relative).expect("staged path");
        let staged = NamespaceEntryEndpoint::new(
            participant_id,
            source_id,
            staged_path,
            NamespaceEntryExpectation::Directory(
                NamespaceDirectoryExpectation::new(7, "staged-directory", None)
                    .expect("staged expectation"),
            ),
        )
        .expect("staged endpoint");
        (participants, original_path, staged, staged_relative)
    }

    fn make_observation() -> RetainedFolderObservation {
        let (_, original, staged, staged_relative) = fixture();
        RetainedFolderObservation::new(
            original,
            staged,
            staged_relative,
            RetainedDeleteId::new(Uuid::new_v4()).expect("retained ID"),
            RetainedResolutionStage::Deleted,
            RetainedRecoveryRootKind::LegacyDeleteStaging,
            fingerprint_selected_entry_json(braced_entry_bytes()).expect("fingerprint"),
        )
        .expect("observation")
    }

    fn braced_entry_bytes() -> &'static [u8] {
        br#"{"id":"retained","original_relative":"drums/kick","staged_relative":"drums/kick","stage":"deleted","restore_stamp":null,"created_at":1}"#
    }

    #[test]
    fn fingerprint_is_domain_separated_stable_and_detects_reused_id_changes() {
        let first = fingerprint_selected_entry_json(braced_entry_bytes()).expect("first");
        let second = fingerprint_selected_entry_json(braced_entry_bytes()).expect("second");
        assert_eq!(first, second);
        assert_ne!(
            first,
            fingerprint_selected_entry_json(
                br#"{"id":"retained","original_relative":"drums/kick","staged_relative":"drums/kick","stage":"deleted","restore_stamp":"changed","created_at":1}"#,
            )
            .expect("changed")
        );
    }

    #[test]
    fn fingerprint_rejects_empty_and_oversized_capture() {
        assert_eq!(
            fingerprint_selected_entry_json(&[]),
            Err(RetainedFingerprintValidationError::Empty)
        );
        assert_eq!(
            fingerprint_selected_entry_json(&vec![
                b'x';
                MAX_RETAINED_SELECTED_ENTRY_JSON_BYTES + 1
            ]),
            Err(RetainedFingerprintValidationError::TooLong {
                actual: MAX_RETAINED_SELECTED_ENTRY_JSON_BYTES + 1,
                maximum: MAX_RETAINED_SELECTED_ENTRY_JSON_BYTES,
            })
        );
    }

    #[test]
    fn restore_and_confirmed_purge_validate_only_deleted_retained_observations() {
        let (participants, _, _, _) = fixture();
        let observation = make_observation();
        let restore = RetainedRestoreIntentArgs::new(observation.clone()).expect("restore");
        restore
            .validate_against_participants(&participants)
            .expect("restore validation");
        assert_eq!(
            restore.collision_policy(),
            RetainedRestoreConflictPolicy::PreserveConflictsKeepBoth
        );
        assert_eq!(observation.original_relative().as_str(), "drums/kick");
        let purge = RetainedPurgeIntentArgs::new(observation, RetainedPurgeDecision::UserConfirmed)
            .expect("purge");
        purge
            .validate_against_participants(&participants)
            .expect("purge validation");
        assert_eq!(
            RetainedPurgeIntentArgs::new(make_observation(), RetainedPurgeDecision::NotConfirmed,),
            Err(RetainedResolutionValidationError::PurgeNotConfirmed)
        );
    }

    #[test]
    fn retained_observation_rejects_reserved_aliases_and_ineligible_stage() {
        let (_, _, staged, staged_relative) = fixture();
        let aliased_original =
            StoredRelativePath::new(".YIELD_DELETE_STAGING/old").expect("aliased original path");
        assert_eq!(
            RetainedFolderObservation::new(
                aliased_original,
                staged.clone(),
                staged_relative.clone(),
                RetainedDeleteId::new(Uuid::new_v4()).expect("ID"),
                RetainedResolutionStage::Deleted,
                RetainedRecoveryRootKind::LegacyDeleteStaging,
                RetainedEntryFingerprint::from_bytes([1; 32]),
            ),
            Err(RetainedResolutionValidationError::RecoveryNamespaceOverlap)
        );
        assert!(matches!(
            RetainedFolderObservation::new(
                StoredRelativePath::new("drums/kick").expect("original path"),
                staged,
                staged_relative,
                RetainedDeleteId::new(Uuid::new_v4()).expect("ID"),
                RetainedResolutionStage::RestorePendingDb,
                RetainedRecoveryRootKind::LegacyDeleteStaging,
                RetainedEntryFingerprint::from_bytes([1; 32]),
            ),
            Err(RetainedResolutionValidationError::IneligibleStage { .. })
        ));
    }

    #[test]
    fn retained_observation_requires_one_fenced_source_and_required_filesystem() {
        let (participants, _, _, _) = fixture();
        let observation = make_observation();
        let participant_value = serde_json::to_value(&participants).expect("participants JSON");

        let mut missing_source = participant_value.clone();
        missing_source["descriptors"]
            .as_array_mut()
            .expect("descriptors")
            .remove(0);
        missing_source["checkpoints"]
            .as_array_mut()
            .expect("checkpoints")
            .remove(0);
        let missing_source: ParticipantSet =
            serde_json::from_value(missing_source).expect("missing source participant");
        assert!(matches!(
            observation.validate_against_participants(&missing_source),
            Err(RetainedResolutionValidationError::StagedEndpoint(
                NamespaceCommandValidationError::ParticipantMissing { .. }
            ))
        ));

        let mut wrong_source = participant_value.clone();
        wrong_source["descriptors"][0]["kind"] = Value::from("Filesystem");
        let wrong_source: ParticipantSet =
            serde_json::from_value(wrong_source).expect("wrong source participant");
        assert!(matches!(
            observation.validate_against_participants(&wrong_source),
            Err(RetainedResolutionValidationError::StagedEndpoint(
                NamespaceCommandValidationError::ParticipantWrongKind { .. }
            ))
        ));

        let mut optional_source = participant_value.clone();
        optional_source["descriptors"][0]["requirement"] = Value::from("OptionalRebuildable");
        let optional_source: ParticipantSet =
            serde_json::from_value(optional_source).expect("optional source participant");
        assert!(matches!(
            observation.validate_against_participants(&optional_source),
            Err(RetainedResolutionValidationError::Participants(_))
        ));

        let mut missing_filesystem = participant_value.clone();
        missing_filesystem["descriptors"]
            .as_array_mut()
            .expect("descriptors")
            .remove(1);
        missing_filesystem["checkpoints"]
            .as_array_mut()
            .expect("checkpoints")
            .remove(1);
        let missing_filesystem: ParticipantSet =
            serde_json::from_value(missing_filesystem).expect("missing filesystem participant");
        assert_eq!(
            observation.validate_against_participants(&missing_filesystem),
            Err(RetainedResolutionValidationError::RequiredFilesystemParticipantMissing)
        );

        let mut optional_filesystem = participant_value.clone();
        optional_filesystem["descriptors"][1]["requirement"] = Value::from("OptionalRebuildable");
        let optional_filesystem: ParticipantSet =
            serde_json::from_value(optional_filesystem).expect("optional filesystem participant");
        assert!(matches!(
            observation.validate_against_participants(&optional_filesystem),
            Err(RetainedResolutionValidationError::Participants(_))
        ));

        let mut duplicate = participant_value;
        let descriptor = duplicate["descriptors"][0].clone();
        duplicate["descriptors"]
            .as_array_mut()
            .expect("descriptors")
            .push(descriptor);
        let duplicate: ParticipantSet =
            serde_json::from_value(duplicate).expect("duplicate participant inventory");
        assert!(matches!(
            observation.validate_against_participants(&duplicate),
            Err(RetainedResolutionValidationError::Participants(_))
        ));
    }

    #[test]
    fn retained_observation_rejects_endpoint_source_or_revision_mismatches_and_file_entries() {
        let (participants, _, staged, staged_relative) = fixture();
        let observation = make_observation();
        let mut source_mismatch = serde_json::to_value(&observation).expect("observation JSON");
        source_mismatch["staged"]["source_id"] = Value::from("other-source");
        let source_mismatch: RetainedFolderObservation =
            serde_json::from_value(source_mismatch).expect("source mismatch observation");
        assert!(matches!(
            source_mismatch.validate_against_participants(&participants),
            Err(RetainedResolutionValidationError::StagedEndpoint(
                NamespaceCommandValidationError::ParticipantSourceMismatch { .. }
            ))
        ));

        let mut revision_mismatch = serde_json::to_value(&observation).expect("observation JSON");
        revision_mismatch["staged"]["expectation"]["Directory"]["source_revision"] =
            Value::from(8_u64);
        let revision_mismatch: RetainedFolderObservation =
            serde_json::from_value(revision_mismatch).expect("revision mismatch observation");
        assert!(matches!(
            revision_mismatch.validate_against_participants(&participants),
            Err(RetainedResolutionValidationError::StagedEndpoint(
                NamespaceCommandValidationError::SourceRevisionMismatch { .. }
            ))
        ));

        let file_endpoint = NamespaceEntryEndpoint::new(
            staged.participant_id(),
            staged.source_id().clone(),
            staged.relative_path().clone(),
            NamespaceEntryExpectation::RegularFile(
                super::super::namespace_commands::NamespaceFileExpectation::new(
                    7,
                    "staged-file",
                    12,
                    None,
                )
                .expect("file expectation"),
            ),
        )
        .expect("file endpoint");
        assert_eq!(
            RetainedFolderObservation::new(
                StoredRelativePath::new("drums/kick").expect("original"),
                file_endpoint,
                staged_relative,
                RetainedDeleteId::new(Uuid::new_v4()).expect("retained ID"),
                RetainedResolutionStage::Deleted,
                RetainedRecoveryRootKind::LegacyDeleteStaging,
                RetainedEntryFingerprint::from_bytes([1; 32]),
            ),
            Err(RetainedResolutionValidationError::StagedMustBeDirectory)
        );
    }

    #[test]
    fn retained_observation_checks_composed_staged_path_bound() {
        let (_, _, staged, _) = fixture();
        let too_long_relative = StoredRelativePath::new(format!(
            "{}x",
            "a".repeat(
                super::super::coordinator::MAX_STORED_RELATIVE_PATH_BYTES
                    - RETAINED_RECOVERY_DIRECTORY.len()
                    - 1
            )
        ))
        .expect("individually bounded staged-relative path");
        assert_eq!(
            RetainedFolderObservation::new(
                StoredRelativePath::new("drums/kick").expect("original"),
                staged,
                too_long_relative,
                RetainedDeleteId::new(Uuid::new_v4()).expect("retained ID"),
                RetainedResolutionStage::Deleted,
                RetainedRecoveryRootKind::LegacyDeleteStaging,
                RetainedEntryFingerprint::from_bytes([1; 32]),
            ),
            Err(RetainedResolutionValidationError::InvalidStagedPath(
                StoredRelativePathError::TooLong
            ))
        );
    }

    #[test]
    fn raw_retained_resolution_boundary_is_strict_and_bounded() {
        let args = RetainedResolutionCommandArgs::Restore(
            RetainedRestoreIntentArgs::new(make_observation()).expect("restore"),
        );
        let value = serde_json::to_value(&args).expect("args JSON");
        validate_persisted_retained_resolution_command_args_value(&value).expect("valid raw value");
        let mut unknown = value.clone();
        unknown["Restore"]["observation"]["unexpected"] = Value::Null;
        assert!(validate_persisted_retained_resolution_command_args_value(&unknown).is_err());
        let mut bad_fingerprint = value;
        bad_fingerprint["Restore"]["observation"]["selected_entry_fingerprint"] =
            Value::Array(vec![Value::from(1_u8)]);
        assert!(
            validate_persisted_retained_resolution_command_args_value(&bad_fingerprint).is_err()
        );
    }
}
