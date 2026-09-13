//! Bounded observations for configured-trash and retained-folder removal commands.
//!
//! These values describe a user's requested removal workflow before any owner opens, moves, or
//! deletes a filesystem entry.  They retain source identities, revisions, and configured-root
//! observations only.  A later prepared record must bind the actual staging locator and durable
//! restore material before an owner mutates the source; this initial intent does not pretend that
//! such material already exists.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use super::coordinator::{
    CopyCollisionPolicy, ExternalRootBindingId, MAX_IMPORT_ROOT_IDENTITY_BYTES,
    StoredRelativePathError,
};
use super::model::UniversalCommandKind;
use super::namespace_commands::{
    NamespaceCommandValidationError, NamespaceEntryEndpoint, NamespaceEntryExpectation,
    NamespaceEntryKind, validate_persisted_namespace_entry_endpoint_value,
};
use super::participants::{
    MAX_SOURCE_IDENTITY_BYTES, ParticipantDescriptor, ParticipantId, ParticipantKind,
    ParticipantRequirement, ParticipantSet, ParticipantValidationError,
};

/// Arguments for the two bounded removal workflows covered by schema V10.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum RemovalCommandArgs {
    /// Move an observed source entry to the configured external trash root.
    Trash(TrashIntentArgs),
    /// Retain an observed source folder in source-local owned recovery storage.
    StageRetainedFolder(StageRetainedFolderIntentArgs),
}

impl RemovalCommandArgs {
    /// Return the existing universal command kind represented by this removal workflow.
    pub const fn command_kind(&self) -> UniversalCommandKind {
        match self {
            Self::Trash(_) => UniversalCommandKind::Trash,
            Self::StageRetainedFolder(_) => UniversalCommandKind::Delete,
        }
    }

    /// Validate command observations against the complete participant inventory.
    pub fn validate_against_participants(
        &self,
        participants: &ParticipantSet,
    ) -> Result<(), RemovalCommandValidationError> {
        match self {
            Self::Trash(args) => args.validate_against_participants(participants),
            Self::StageRetainedFolder(args) => args.validate_against_participants(participants),
        }
    }
}

/// Initial configured-trash request for one source file or folder.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrashIntentArgs {
    source: NamespaceEntryEndpoint,
    external_root_binding_id: ExternalRootBindingId,
    filesystem_participant_id: ParticipantId,
    trash_root_identity: String,
}

impl TrashIntentArgs {
    /// Construct a configured-trash observation without opening either root or retaining a handle.
    pub fn new(
        source: NamespaceEntryEndpoint,
        external_root_binding_id: ExternalRootBindingId,
        filesystem_participant_id: ParticipantId,
        trash_root_identity: impl Into<String>,
    ) -> Result<Self, RemovalCommandValidationError> {
        let args = Self {
            source,
            external_root_binding_id,
            filesystem_participant_id,
            trash_root_identity: trash_root_identity.into(),
        };
        args.validate_shape()?;
        Ok(args)
    }

    /// Validate source and configured-root observations against the complete participant set.
    pub fn validate_against_participants(
        &self,
        participants: &ParticipantSet,
    ) -> Result<(), RemovalCommandValidationError> {
        self.validate_shape()?;
        participants.validate_for_new_intent()?;
        let source_descriptor = validate_source_participant(participants, &self.source)?;
        validate_required_filesystem_participant(participants, self.filesystem_participant_id)?;

        // A directory whose captured physical identity is the configured trash root is a known
        // self-target. Other containment relationships require a later live root-bound check.
        if let NamespaceEntryExpectation::Directory(expectation) = self.source.expectation()
            && expectation.filesystem_identity() == self.trash_root_identity
        {
            return Err(RemovalCommandValidationError::KnownTrashRootSource);
        }
        // A non-root source-relative path under the source participant's fenced root is also
        // known to be inside the configured trash root when the captured root identities match.
        // This is separate from the exact directory-identity self-target above; nested physical
        // containment still requires a later live root-bound check when identities differ.
        if !self.source.relative_path().as_str().is_empty()
            && source_descriptor
                .source_fence()
                .is_some_and(|fence| fence.root_identity() == self.trash_root_identity)
        {
            return Err(RemovalCommandValidationError::KnownTrashRootNamespace);
        }
        Ok(())
    }

    /// Return the observed source endpoint.
    pub fn source(&self) -> &NamespaceEntryEndpoint {
        &self.source
    }

    /// Return the opaque configured-root binding identity.
    pub const fn external_root_binding_id(&self) -> ExternalRootBindingId {
        self.external_root_binding_id
    }

    /// Return the required Filesystem participant identity.
    pub const fn filesystem_participant_id(&self) -> ParticipantId {
        self.filesystem_participant_id
    }

    /// Return the configured trash-root identity observed at intent capture.
    pub fn trash_root_identity(&self) -> &str {
        &self.trash_root_identity
    }

    /// Return the fixed no-replace destination policy.
    pub const fn collision_policy(&self) -> CopyCollisionPolicy {
        CopyCollisionPolicy::NoReplace
    }

    fn validate_shape(&self) -> Result<(), RemovalCommandValidationError> {
        validate_source_endpoint_shape(&self.source, false)?;
        validate_root_identity(&self.trash_root_identity)
    }
}

/// Initial request to retain a non-root source folder in owned source-local recovery storage.
///
/// The later prepared destination must use a qualified no-replace claim. An occupied recovery
/// target is preserved, never overwritten, even though its locator is not chosen by this intent.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StageRetainedFolderIntentArgs {
    source: NamespaceEntryEndpoint,
}

impl StageRetainedFolderIntentArgs {
    /// Construct a retained-folder observation without selecting a staging locator or recovery
    /// record. The containing operation's [`super::model::OperationId`] is the future recovery
    /// correlation key; this intent does not manufacture a second identity.
    pub fn new(source: NamespaceEntryEndpoint) -> Result<Self, RemovalCommandValidationError> {
        let args = Self { source };
        args.validate_shape()?;
        Ok(args)
    }

    /// Validate the source folder and required fenced source/database participants.
    pub fn validate_against_participants(
        &self,
        participants: &ParticipantSet,
    ) -> Result<(), RemovalCommandValidationError> {
        self.validate_shape()?;
        participants.validate_for_new_intent()?;
        validate_source_participant(participants, &self.source)?;
        if self.source.expectation().kind() != NamespaceEntryKind::Directory {
            return Err(RemovalCommandValidationError::SourceMustBeDirectory);
        }
        validate_required_filesystem_participant_any(participants)
    }

    /// Return the observed source endpoint.
    pub fn source(&self) -> &NamespaceEntryEndpoint {
        &self.source
    }

    /// Return the fixed no-replace policy for the later prepared recovery destination.
    pub const fn collision_policy(&self) -> CopyCollisionPolicy {
        CopyCollisionPolicy::NoReplace
    }

    fn validate_shape(&self) -> Result<(), RemovalCommandValidationError> {
        validate_source_endpoint_shape(&self.source, true)?;
        if self.source.expectation().kind() != NamespaceEntryKind::Directory {
            return Err(RemovalCommandValidationError::SourceMustBeDirectory);
        }
        Ok(())
    }
}

/// Failure while constructing or validating a schema-V10 removal observation.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum RemovalCommandValidationError {
    /// A source-relative locator was malformed.
    #[error("invalid removal source path: {0}")]
    InvalidPath(#[from] StoredRelativePathError),
    /// A decoded source endpoint carried an invalid namespace shape.
    #[error("invalid removal source endpoint: {0}")]
    SourceEndpoint(#[source] NamespaceCommandValidationError),
    /// A source identity was empty, oversized, or contained a control character.
    #[error("removal source identity is invalid")]
    InvalidSourceIdentity,
    /// The configured trash root identity was outside its bounded text contract.
    #[error("removal trash-root identity is invalid")]
    InvalidTrashRootIdentity,
    /// A retained-folder request attempted to remove a regular file.
    #[error("retained-folder staging requires a directory source")]
    SourceMustBeDirectory,
    /// A retained-folder request named the source root rather than a child folder.
    #[error("retained-folder staging cannot target a source root")]
    SourceRootForbidden,
    /// The configured trash root is the same known directory as the source.
    #[error("trash source is the configured trash root")]
    KnownTrashRootSource,
    /// The configured trash root is the source participant's fenced namespace.
    #[error("trash source is inside the configured trash root namespace")]
    KnownTrashRootNamespace,
    /// The source endpoint participant was absent.
    #[error("removal source participant is missing")]
    SourceParticipantMissing,
    /// The source endpoint participant had another kind.
    #[error("removal source participant has kind {actual:?}, expected SourceDatabase")]
    SourceParticipantWrongKind {
        /// Actual participant kind observed in the inventory.
        actual: ParticipantKind,
    },
    /// The source endpoint participant was not required.
    #[error("removal source participant must be required")]
    SourceParticipantNotRequired,
    /// The source participant lacked its required fence.
    #[error("removal source participant is missing its source fence")]
    SourceParticipantFenceMissing,
    /// The source ID differed from the fenced participant source.
    #[error("removal source ID does not match the source participant fence")]
    SourceParticipantSourceMismatch,
    /// The source revision differed from the fenced participant revision.
    #[error("removal source revision does not match the source participant fence")]
    SourceRevisionMismatch,
    /// The configured external filesystem participant was absent.
    #[error("removal Filesystem participant is missing")]
    FilesystemParticipantMissing,
    /// The configured external filesystem participant had another kind.
    #[error("removal Filesystem participant has kind {actual:?}")]
    FilesystemParticipantWrongKind {
        /// Actual participant kind observed in the inventory.
        actual: ParticipantKind,
    },
    /// The configured external filesystem participant was not required.
    #[error("removal Filesystem participant must be required")]
    FilesystemParticipantNotRequired,
    /// No required Filesystem participant was present for source-local retention.
    #[error("retained-folder staging requires a required Filesystem participant")]
    RequiredFilesystemParticipantMissing,
    /// The complete participant inventory failed its stored shape checks.
    #[error("invalid removal participants: {0}")]
    Participants(#[from] ParticipantValidationError),
}

fn validate_source_endpoint_shape(
    source: &NamespaceEntryEndpoint,
    forbid_root: bool,
) -> Result<(), RemovalCommandValidationError> {
    if forbid_root && source.relative_path().as_str().is_empty() {
        return Err(RemovalCommandValidationError::SourceRootForbidden);
    }
    source
        .relative_path()
        .validate()
        .map_err(RemovalCommandValidationError::InvalidPath)?;
    if source.source_id().as_str().is_empty()
        || source.source_id().as_str().len() > MAX_SOURCE_IDENTITY_BYTES
        || source.source_id().as_str().chars().any(char::is_control)
    {
        return Err(RemovalCommandValidationError::InvalidSourceIdentity);
    }
    source
        .expectation()
        .validate()
        .map_err(RemovalCommandValidationError::SourceEndpoint)
}

fn validate_source_participant<'a>(
    participants: &'a ParticipantSet,
    source: &NamespaceEntryEndpoint,
) -> Result<&'a ParticipantDescriptor, RemovalCommandValidationError> {
    let descriptor = participants
        .descriptors()
        .iter()
        .find(|descriptor| descriptor.id() == source.participant_id())
        .ok_or(RemovalCommandValidationError::SourceParticipantMissing)?;
    if descriptor.kind() != ParticipantKind::SourceDatabase {
        return Err(RemovalCommandValidationError::SourceParticipantWrongKind {
            actual: descriptor.kind(),
        });
    }
    if descriptor.requirement() != ParticipantRequirement::Required {
        return Err(RemovalCommandValidationError::SourceParticipantNotRequired);
    }
    let fence = descriptor
        .source_fence()
        .ok_or(RemovalCommandValidationError::SourceParticipantFenceMissing)?;
    if fence.source_id() != source.source_id() {
        return Err(RemovalCommandValidationError::SourceParticipantSourceMismatch);
    }
    if fence.source_revision() != source.expectation().source_revision() {
        return Err(RemovalCommandValidationError::SourceRevisionMismatch);
    }
    Ok(descriptor)
}

fn validate_required_filesystem_participant(
    participants: &ParticipantSet,
    participant_id: ParticipantId,
) -> Result<(), RemovalCommandValidationError> {
    let descriptor = participants
        .descriptors()
        .iter()
        .find(|descriptor| descriptor.id() == participant_id)
        .ok_or(RemovalCommandValidationError::FilesystemParticipantMissing)?;
    if descriptor.kind() != ParticipantKind::Filesystem {
        return Err(
            RemovalCommandValidationError::FilesystemParticipantWrongKind {
                actual: descriptor.kind(),
            },
        );
    }
    if descriptor.requirement() != ParticipantRequirement::Required {
        return Err(RemovalCommandValidationError::FilesystemParticipantNotRequired);
    }
    Ok(())
}

fn validate_required_filesystem_participant_any(
    participants: &ParticipantSet,
) -> Result<(), RemovalCommandValidationError> {
    if participants.descriptors().iter().any(|descriptor| {
        descriptor.kind() == ParticipantKind::Filesystem
            && descriptor.requirement() == ParticipantRequirement::Required
    }) {
        Ok(())
    } else {
        Err(RemovalCommandValidationError::RequiredFilesystemParticipantMissing)
    }
}

fn validate_root_identity(value: &str) -> Result<(), RemovalCommandValidationError> {
    if value.is_empty()
        || value.len() > MAX_IMPORT_ROOT_IDENTITY_BYTES
        || value.chars().any(char::is_control)
    {
        Err(RemovalCommandValidationError::InvalidTrashRootIdentity)
    } else {
        Ok(())
    }
}

/// Validate a tagged V10 removal value before typed deserialization.
pub(crate) fn validate_persisted_removal_command_args_value(
    value: &Value,
) -> Result<(), &'static str> {
    let object = strict_object(value, &["Trash", "StageRetainedFolder"])?;
    if object.len() != 1 {
        return Err("persisted removal command must contain exactly one variant");
    }
    let (variant, arguments) = object
        .iter()
        .next()
        .ok_or("persisted removal command must contain exactly one variant")?;
    match variant.as_str() {
        "Trash" => validate_persisted_trash_value(arguments),
        "StageRetainedFolder" => validate_persisted_stage_retained_folder_value(arguments),
        _ => Err("persisted removal command contains an unknown variant"),
    }
}

fn validate_persisted_trash_value(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(
        value,
        &[
            "source",
            "external_root_binding_id",
            "filesystem_participant_id",
            "trash_root_identity",
        ],
    )?;
    for field in [
        "source",
        "external_root_binding_id",
        "filesystem_participant_id",
        "trash_root_identity",
    ] {
        required_field(object, field)?;
    }
    validate_persisted_namespace_entry_endpoint_value(required_field(object, "source")?)?;
    validate_uuid(
        required_field(object, "external_root_binding_id")?,
        "external root binding",
    )?;
    validate_uuid(
        required_field(object, "filesystem_participant_id")?,
        "filesystem participant",
    )?;
    let root = required_field(object, "trash_root_identity")?
        .as_str()
        .ok_or("trash root identity must be a string")?;
    if root.is_empty()
        || root.len() > MAX_IMPORT_ROOT_IDENTITY_BYTES
        || root.chars().any(char::is_control)
    {
        return Err("trash root identity is outside its bound");
    }
    Ok(())
}

fn validate_persisted_stage_retained_folder_value(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(value, &["source"])?;
    validate_persisted_namespace_entry_endpoint_value(required_field(object, "source")?)
}

fn validate_uuid(value: &Value, name: &str) -> Result<(), &'static str> {
    if value
        .as_str()
        .and_then(|value| uuid::Uuid::parse_str(value).ok())
        .is_some()
    {
        Ok(())
    } else if name == "external root binding" {
        Err("external root binding ID must be a UUID string")
    } else {
        Err("filesystem participant ID must be a UUID string")
    }
}

fn strict_object<'a>(
    value: &'a Value,
    allowed_fields: &[&str],
) -> Result<&'a serde_json::Map<String, Value>, &'static str> {
    let object = value
        .as_object()
        .ok_or("persisted removal value must be a JSON object")?;
    if object
        .keys()
        .any(|field| !allowed_fields.contains(&field.as_str()))
    {
        return Err("persisted removal object contains an unknown field");
    }
    Ok(object)
}

fn required_field<'a>(
    object: &'a serde_json::Map<String, Value>,
    field: &str,
) -> Result<&'a Value, &'static str> {
    object
        .get(field)
        .ok_or("persisted removal object is missing a required field")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operation_journal::{
        NamespaceDirectoryExpectation, NamespaceFileExpectation, ParticipantCheckpoint,
        ParticipantCheckpointState, StoredSourceFence,
    };
    use crate::sample_sources::SourceId;
    use uuid::Uuid;

    fn fixture(
        expectation: NamespaceEntryExpectation,
    ) -> (
        ParticipantSet,
        NamespaceEntryEndpoint,
        ParticipantId,
        ParticipantId,
    ) {
        let source_id = SourceId::from_string("source-a");
        let source_participant_id = ParticipantId::from_uuid(Uuid::from_u128(1));
        let filesystem_participant_id = ParticipantId::from_uuid(Uuid::from_u128(2));
        let fence = StoredSourceFence::new(source_id.clone(), "source-root", "source-db", 1, 7, 1)
            .expect("source fence");
        let participants = ParticipantSet::new(
            vec![
                ParticipantDescriptor::new(
                    source_participant_id,
                    ParticipantKind::SourceDatabase,
                    ParticipantRequirement::Required,
                    Some(fence),
                    None,
                )
                .expect("source descriptor"),
                ParticipantDescriptor::new(
                    filesystem_participant_id,
                    ParticipantKind::Filesystem,
                    ParticipantRequirement::Required,
                    None,
                    None,
                )
                .expect("filesystem descriptor"),
            ],
            vec![
                ParticipantCheckpoint::new(
                    source_participant_id,
                    ParticipantCheckpointState::Pending,
                    None,
                    None,
                )
                .expect("source checkpoint"),
                ParticipantCheckpoint::new(
                    filesystem_participant_id,
                    ParticipantCheckpointState::Pending,
                    None,
                    None,
                )
                .expect("filesystem checkpoint"),
            ],
        )
        .expect("participants");
        let endpoint = NamespaceEntryEndpoint::new(
            source_participant_id,
            source_id,
            super::super::coordinator::StoredRelativePath::new("folder/item.wav")
                .expect("source path"),
            expectation,
        )
        .expect("source endpoint");
        (
            participants,
            endpoint,
            source_participant_id,
            filesystem_participant_id,
        )
    }

    #[test]
    fn trash_accepts_file_and_folder_observations_with_fixed_no_replace() {
        for expectation in [
            NamespaceEntryExpectation::RegularFile(
                NamespaceFileExpectation::new(7, "file-identity", 12, Some(4))
                    .expect("file expectation"),
            ),
            NamespaceEntryExpectation::Directory(
                NamespaceDirectoryExpectation::new(7, "directory-identity", Some(4))
                    .expect("directory expectation"),
            ),
        ] {
            let (participants, endpoint, _, filesystem_id) = fixture(expectation);
            let args = TrashIntentArgs::new(
                endpoint,
                ExternalRootBindingId::from_uuid(Uuid::from_u128(3)),
                filesystem_id,
                "trash-root",
            )
            .expect("trash args");
            args.validate_against_participants(&participants)
                .expect("trash validation");
            assert_eq!(args.collision_policy(), CopyCollisionPolicy::NoReplace);
        }
    }

    #[test]
    fn trash_rejects_known_trash_root_directory() {
        let (participants, endpoint, _, filesystem_id) =
            fixture(NamespaceEntryExpectation::Directory(
                NamespaceDirectoryExpectation::new(7, "trash-root", None)
                    .expect("directory expectation"),
            ));
        let args = TrashIntentArgs::new(
            endpoint,
            ExternalRootBindingId::from_uuid(Uuid::from_u128(3)),
            filesystem_id,
            "trash-root",
        )
        .expect("trash args");
        assert_eq!(
            args.validate_against_participants(&participants),
            Err(RemovalCommandValidationError::KnownTrashRootSource)
        );
    }

    #[test]
    fn trash_rejects_non_root_sources_in_the_fenced_trash_namespace() {
        for expectation in [
            NamespaceEntryExpectation::RegularFile(
                NamespaceFileExpectation::new(7, "file-identity", 12, None)
                    .expect("file expectation"),
            ),
            NamespaceEntryExpectation::Directory(
                NamespaceDirectoryExpectation::new(7, "directory-identity", None)
                    .expect("directory expectation"),
            ),
        ] {
            let (participants, endpoint, _, filesystem_id) = fixture(expectation);
            let args = TrashIntentArgs::new(
                endpoint,
                ExternalRootBindingId::from_uuid(Uuid::from_u128(3)),
                filesystem_id,
                "source-root",
            )
            .expect("trash args");
            assert_eq!(
                args.validate_against_participants(&participants),
                Err(RemovalCommandValidationError::KnownTrashRootNamespace)
            );
        }
    }

    #[test]
    fn removal_validation_rejects_missing_wrong_optional_and_duplicate_participants() {
        let (participants, endpoint, _, filesystem_id) =
            fixture(NamespaceEntryExpectation::RegularFile(
                NamespaceFileExpectation::new(7, "file-identity", 12, None)
                    .expect("file expectation"),
            ));
        let args = TrashIntentArgs::new(
            endpoint,
            ExternalRootBindingId::from_uuid(Uuid::from_u128(3)),
            filesystem_id,
            "trash-root",
        )
        .expect("trash args");
        let mut inventory = serde_json::to_value(&participants).expect("participant JSON");

        let mut missing_filesystem = inventory.clone();
        missing_filesystem["descriptors"]
            .as_array_mut()
            .unwrap()
            .remove(1);
        missing_filesystem["checkpoints"]
            .as_array_mut()
            .unwrap()
            .remove(1);
        let missing_filesystem: ParticipantSet =
            serde_json::from_value(missing_filesystem).expect("missing filesystem inventory");
        assert_eq!(
            args.validate_against_participants(&missing_filesystem),
            Err(RemovalCommandValidationError::FilesystemParticipantMissing)
        );

        inventory["descriptors"][1]["kind"] = Value::String(String::from("GlobalLibrary"));
        let wrong_filesystem: ParticipantSet =
            serde_json::from_value(inventory.clone()).expect("wrong filesystem inventory");
        assert!(matches!(
            args.validate_against_participants(&wrong_filesystem),
            Err(RemovalCommandValidationError::FilesystemParticipantWrongKind { .. })
        ));

        inventory["descriptors"][1]["kind"] = Value::String(String::from("Filesystem"));
        inventory["descriptors"][1]["requirement"] =
            Value::String(String::from("OptionalRebuildable"));
        let optional_filesystem: ParticipantSet =
            serde_json::from_value(inventory.clone()).expect("optional filesystem inventory");
        assert!(matches!(
            args.validate_against_participants(&optional_filesystem),
            Err(RemovalCommandValidationError::Participants(_))
        ));

        inventory["descriptors"][1]["requirement"] = Value::String(String::from("Required"));
        inventory["descriptors"][1]["id"] = inventory["descriptors"][0]["id"].clone();
        let duplicate: ParticipantSet =
            serde_json::from_value(inventory).expect("duplicate participant inventory");
        assert!(matches!(
            args.validate_against_participants(&duplicate),
            Err(RemovalCommandValidationError::Participants(_))
        ));
    }

    #[test]
    fn removal_validation_rejects_source_identity_and_revision_mismatches() {
        let (participants, endpoint, _, filesystem_id) =
            fixture(NamespaceEntryExpectation::RegularFile(
                NamespaceFileExpectation::new(7, "file-identity", 12, None)
                    .expect("file expectation"),
            ));
        let args = TrashIntentArgs::new(
            endpoint,
            ExternalRootBindingId::from_uuid(Uuid::from_u128(3)),
            filesystem_id,
            "trash-root",
        )
        .expect("trash args");

        let mut bad_source_id = serde_json::to_value(&args).expect("trash JSON");
        bad_source_id["source"]["source_id"] = Value::String(String::new());
        let bad_source_id: TrashIntentArgs =
            serde_json::from_value(bad_source_id).expect("decoded bad source ID");
        assert_eq!(
            bad_source_id.validate_against_participants(&participants),
            Err(RemovalCommandValidationError::InvalidSourceIdentity)
        );

        let mut bad_revision = serde_json::to_value(&args).expect("trash JSON");
        bad_revision["source"]["expectation"]["RegularFile"]["source_revision"] =
            Value::from(8_u64);
        let bad_revision: TrashIntentArgs =
            serde_json::from_value(bad_revision).expect("decoded bad source revision");
        assert_eq!(
            bad_revision.validate_against_participants(&participants),
            Err(RemovalCommandValidationError::SourceRevisionMismatch)
        );
    }

    #[test]
    fn retained_folder_accepts_directory_and_rejects_file() {
        let (participants, endpoint, _, _) = fixture(NamespaceEntryExpectation::Directory(
            NamespaceDirectoryExpectation::new(7, "directory-identity", None)
                .expect("directory expectation"),
        ));
        let args = StageRetainedFolderIntentArgs::new(endpoint).expect("stage args");
        args.validate_against_participants(&participants)
            .expect("stage validation");
        assert_eq!(args.collision_policy(), CopyCollisionPolicy::NoReplace);

        let (participants, endpoint, _, _) = fixture(NamespaceEntryExpectation::RegularFile(
            NamespaceFileExpectation::new(7, "file-identity", 12, None).expect("file expectation"),
        ));
        assert_eq!(
            StageRetainedFolderIntentArgs::new(endpoint),
            Err(RemovalCommandValidationError::SourceMustBeDirectory)
        );
        let _ = participants;
    }
}
