//! Bounded namespace mutation command observations.
//!
//! These values describe one file or folder rename, move, or folder-creation request.  They
//! contain only source identities, revisions, and portable relative locators.  They do not retain
//! an open source, a filesystem capability, a folder inventory, or permission to perform the
//! eventual mutation.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use super::coordinator::{CopyCollisionPolicy, StoredRelativePath, StoredRelativePathError};
use super::model::UniversalCommandKind;
use super::participants::{
    MAX_SOURCE_IDENTITY_BYTES, ParticipantDescriptor, ParticipantId, ParticipantKind,
    ParticipantRequirement, ParticipantSet, ParticipantValidationError,
};
use crate::sample_sources::SourceId;

/// Maximum byte length of a stored namespace-entry identity observation.
pub const MAX_NAMESPACE_ENTRY_IDENTITY_BYTES: usize = 256;
/// Maximum byte length of one namespace destination leaf.
pub const MAX_NAMESPACE_LEAF_NAME_BYTES: usize = 256;

/// Whether a namespace entry is a regular file or a directory.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum NamespaceEntryKind {
    /// A regular file with bounded identity and size observations.
    RegularFile,
    /// A directory with a bounded physical identity observation.
    Directory,
}

/// A bounded regular-file observation captured at the source owner boundary.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NamespaceFileExpectation {
    source_revision: u64,
    filesystem_identity: String,
    byte_len: u64,
    modified_unix_ns: Option<i64>,
}

impl NamespaceFileExpectation {
    /// Construct a regular-file observation without opening the file or acquiring ownership.
    pub fn new(
        source_revision: u64,
        filesystem_identity: impl Into<String>,
        byte_len: u64,
        modified_unix_ns: Option<i64>,
    ) -> Result<Self, NamespaceCommandValidationError> {
        let expectation = Self {
            source_revision,
            filesystem_identity: filesystem_identity.into(),
            byte_len,
            modified_unix_ns,
        };
        expectation.validate()?;
        Ok(expectation)
    }

    /// Validate an observation recovered from untrusted journal bytes.
    pub fn validate(&self) -> Result<(), NamespaceCommandValidationError> {
        validate_identity(&self.filesystem_identity)
    }

    /// Return the source revision observed with this file.
    pub const fn source_revision(&self) -> u64 {
        self.source_revision
    }

    /// Return the opaque physical file identity observation.
    pub fn filesystem_identity(&self) -> &str {
        &self.filesystem_identity
    }

    /// Return the observed regular-file length.
    pub const fn byte_len(&self) -> u64 {
        self.byte_len
    }

    /// Return the optional signed modification timestamp.
    pub const fn modified_unix_ns(&self) -> Option<i64> {
        self.modified_unix_ns
    }
}

/// A bounded directory observation captured at the source owner boundary.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NamespaceDirectoryExpectation {
    source_revision: u64,
    filesystem_identity: String,
    modified_unix_ns: Option<i64>,
}

impl NamespaceDirectoryExpectation {
    /// Construct a directory observation without opening the directory or acquiring ownership.
    pub fn new(
        source_revision: u64,
        filesystem_identity: impl Into<String>,
        modified_unix_ns: Option<i64>,
    ) -> Result<Self, NamespaceCommandValidationError> {
        let expectation = Self {
            source_revision,
            filesystem_identity: filesystem_identity.into(),
            modified_unix_ns,
        };
        expectation.validate()?;
        Ok(expectation)
    }

    /// Validate an observation recovered from untrusted journal bytes.
    pub fn validate(&self) -> Result<(), NamespaceCommandValidationError> {
        validate_identity(&self.filesystem_identity)
    }

    /// Return the source revision observed with this directory.
    pub const fn source_revision(&self) -> u64 {
        self.source_revision
    }

    /// Return the opaque physical directory identity observation.
    pub fn filesystem_identity(&self) -> &str {
        &self.filesystem_identity
    }

    /// Return the optional signed modification timestamp.
    pub const fn modified_unix_ns(&self) -> Option<i64> {
        self.modified_unix_ns
    }
}

/// Expected type and bounded identity of an existing namespace entry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum NamespaceEntryExpectation {
    /// The source must still identify the same regular file.
    RegularFile(NamespaceFileExpectation),
    /// The source must still identify the same directory.
    Directory(NamespaceDirectoryExpectation),
}

impl NamespaceEntryExpectation {
    /// Return the expected entry kind.
    pub const fn kind(&self) -> NamespaceEntryKind {
        match self {
            Self::RegularFile(_) => NamespaceEntryKind::RegularFile,
            Self::Directory(_) => NamespaceEntryKind::Directory,
        }
    }

    /// Return the source revision captured by the expectation.
    pub const fn source_revision(&self) -> u64 {
        match self {
            Self::RegularFile(expectation) => expectation.source_revision(),
            Self::Directory(expectation) => expectation.source_revision(),
        }
    }

    /// Validate the tagged expectation recovered from storage.
    pub fn validate(&self) -> Result<(), NamespaceCommandValidationError> {
        match self {
            Self::RegularFile(expectation) => expectation.validate(),
            Self::Directory(expectation) => expectation.validate(),
        }
    }
}

/// A source-database locator for a namespace directory.  `Root` is the source root itself and is
/// valid only as a parent; a mutation source must always use a nonempty child path.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum NamespaceDirectoryLocator {
    /// The source root represented by the referenced source fence.
    Root,
    /// A nonempty path below the source root.
    Child(StoredRelativePath),
}

impl NamespaceDirectoryLocator {
    /// Construct a root directory locator.
    pub const fn root() -> Self {
        Self::Root
    }

    /// Construct a child directory locator.
    pub fn child(path: StoredRelativePath) -> Result<Self, NamespaceCommandValidationError> {
        path.validate()
            .map_err(NamespaceCommandValidationError::InvalidPath)?;
        Ok(Self::Child(path))
    }

    /// Validate a locator recovered from storage.
    pub fn validate(&self) -> Result<(), NamespaceCommandValidationError> {
        match self {
            Self::Root => Ok(()),
            Self::Child(path) => path
                .validate()
                .map_err(NamespaceCommandValidationError::InvalidPath),
        }
    }

    /// Borrow the child path, if this is not the source root.
    pub fn child_path(&self) -> Option<&StoredRelativePath> {
        match self {
            Self::Root => None,
            Self::Child(path) => Some(path),
        }
    }
}

/// A bounded single path component used as a rename, move, or folder-creation leaf.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct NamespaceLeafName(String);

impl NamespaceLeafName {
    /// Construct a portable nonempty leaf name.
    pub fn new(value: impl Into<String>) -> Result<Self, NamespaceCommandValidationError> {
        let leaf = Self(value.into());
        leaf.validate()?;
        Ok(leaf)
    }

    /// Borrow the portable leaf spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Validate a leaf recovered from untrusted storage.
    pub fn validate(&self) -> Result<(), NamespaceCommandValidationError> {
        if self.0.is_empty() {
            return Err(NamespaceCommandValidationError::InvalidLeaf(
                NamespaceLeafViolation::Empty,
            ));
        }
        if self.0.len() > MAX_NAMESPACE_LEAF_NAME_BYTES {
            return Err(NamespaceCommandValidationError::InvalidLeaf(
                NamespaceLeafViolation::TooLong,
            ));
        }
        if self.0.as_bytes().contains(&0) {
            return Err(NamespaceCommandValidationError::InvalidLeaf(
                NamespaceLeafViolation::EmbeddedNul,
            ));
        }
        if self.0.contains(['/', '\\']) {
            return Err(NamespaceCommandValidationError::InvalidLeaf(
                NamespaceLeafViolation::Separator,
            ));
        }
        if self.0 == "." || self.0 == ".." {
            return Err(NamespaceCommandValidationError::InvalidLeaf(
                NamespaceLeafViolation::DotComponent,
            ));
        }
        if self.0.contains(':') {
            return Err(NamespaceCommandValidationError::InvalidLeaf(
                NamespaceLeafViolation::PlatformPrefix,
            ));
        }
        if self.0.chars().any(char::is_control) {
            return Err(NamespaceCommandValidationError::InvalidLeaf(
                NamespaceLeafViolation::ControlCharacter,
            ));
        }
        Ok(())
    }
}

/// Source-database endpoint for an existing file or directory.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NamespaceEntryEndpoint {
    participant_id: ParticipantId,
    source_id: SourceId,
    relative_path: StoredRelativePath,
    expectation: NamespaceEntryExpectation,
}

impl NamespaceEntryEndpoint {
    /// Construct an existing-entry observation without acquiring a live capability.
    pub fn new(
        participant_id: ParticipantId,
        source_id: SourceId,
        relative_path: StoredRelativePath,
        expectation: NamespaceEntryExpectation,
    ) -> Result<Self, NamespaceCommandValidationError> {
        let endpoint = Self {
            participant_id,
            source_id,
            relative_path,
            expectation,
        };
        endpoint.validate_shape()?;
        Ok(endpoint)
    }

    /// Return the participant this endpoint must resolve through.
    pub const fn participant_id(&self) -> ParticipantId {
        self.participant_id
    }

    /// Return the configured source identity observation.
    pub fn source_id(&self) -> &SourceId {
        &self.source_id
    }

    /// Return the nonempty source-relative path.
    pub fn relative_path(&self) -> &StoredRelativePath {
        &self.relative_path
    }

    /// Return the expected entry kind and identity observation.
    pub fn expectation(&self) -> &NamespaceEntryExpectation {
        &self.expectation
    }

    fn validate_shape(&self) -> Result<(), NamespaceCommandValidationError> {
        validate_source_id(&self.source_id)?;
        self.relative_path
            .validate()
            .map_err(NamespaceCommandValidationError::InvalidPath)?;
        self.expectation.validate()
    }
}

/// Source-database endpoint for a directory used as a command parent.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NamespaceDirectoryEndpoint {
    participant_id: ParticipantId,
    source_id: SourceId,
    locator: NamespaceDirectoryLocator,
    expectation: NamespaceDirectoryExpectation,
}

impl NamespaceDirectoryEndpoint {
    /// Construct a parent-directory observation without acquiring a live capability.
    pub fn new(
        participant_id: ParticipantId,
        source_id: SourceId,
        locator: NamespaceDirectoryLocator,
        expectation: NamespaceDirectoryExpectation,
    ) -> Result<Self, NamespaceCommandValidationError> {
        let endpoint = Self {
            participant_id,
            source_id,
            locator,
            expectation,
        };
        endpoint.validate_shape()?;
        Ok(endpoint)
    }

    /// Return the participant this endpoint must resolve through.
    pub const fn participant_id(&self) -> ParticipantId {
        self.participant_id
    }

    /// Return the configured source identity observation.
    pub fn source_id(&self) -> &SourceId {
        &self.source_id
    }

    /// Return the root or child directory locator.
    pub fn locator(&self) -> &NamespaceDirectoryLocator {
        &self.locator
    }

    /// Return the observed parent directory identity.
    pub fn expectation(&self) -> &NamespaceDirectoryExpectation {
        &self.expectation
    }

    fn validate_shape(&self) -> Result<(), NamespaceCommandValidationError> {
        validate_source_id(&self.source_id)?;
        self.locator.validate()?;
        self.expectation.validate()
    }
}

/// Destination parent plus a bounded new leaf name.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NamespaceTarget {
    parent: NamespaceDirectoryEndpoint,
    leaf: NamespaceLeafName,
}

impl NamespaceTarget {
    /// Construct a destination observation with the explicit no-replace policy supplied by the
    /// command kind.  The destination entry itself is intentionally not expected to exist.
    pub fn new(
        parent: NamespaceDirectoryEndpoint,
        leaf: NamespaceLeafName,
    ) -> Result<Self, NamespaceCommandValidationError> {
        let target = Self { parent, leaf };
        target.validate_shape()?;
        validate_composed_target_path(target.parent.locator(), &target.leaf)?;
        Ok(target)
    }

    /// Return the destination parent endpoint.
    pub fn parent(&self) -> &NamespaceDirectoryEndpoint {
        &self.parent
    }

    /// Return the destination leaf.
    pub fn leaf(&self) -> &NamespaceLeafName {
        &self.leaf
    }

    fn validate_shape(&self) -> Result<(), NamespaceCommandValidationError> {
        self.parent.validate_shape()?;
        self.leaf.validate()
    }
}

/// One same-source rename of a regular file or child directory.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NamespaceRenameIntentArgs {
    source: NamespaceEntryEndpoint,
    destination: NamespaceTarget,
}

impl NamespaceRenameIntentArgs {
    /// Construct a same-parent rename observation.
    pub fn new(
        source: NamespaceEntryEndpoint,
        destination: NamespaceTarget,
    ) -> Result<Self, NamespaceCommandValidationError> {
        let args = Self {
            source,
            destination,
        };
        args.validate_shape()?;
        validate_composed_target_path(
            args.destination.parent().locator(),
            args.destination.leaf(),
        )?;
        Ok(args)
    }

    /// Return the existing source endpoint.
    pub fn source(&self) -> &NamespaceEntryEndpoint {
        &self.source
    }

    /// Return the destination parent and leaf.
    pub fn destination(&self) -> &NamespaceTarget {
        &self.destination
    }

    /// Validate endpoint relationships against the retained participant observations.
    pub fn validate_against_participants(
        &self,
        participants: &ParticipantSet,
    ) -> Result<(), NamespaceCommandValidationError> {
        participants.validate_for_new_intent()?;
        self.validate_shape()?;
        let source =
            validate_entry_endpoint(participants, NamespaceEndpointRole::Source, &self.source)?;
        let destination = validate_directory_endpoint(
            participants,
            NamespaceEndpointRole::Destination,
            self.destination.parent(),
        )?;
        require_filesystem_participant(participants)?;

        if source.id() != destination.id()
            || source
                .source_fence()
                .expect("validated source fence")
                .source_id()
                != self.destination.parent().source_id()
            || self.source_parent_locator() != *self.destination.parent().locator()
        {
            return Err(NamespaceCommandValidationError::RenameRequiresSameParent);
        }
        if source
            .source_fence()
            .expect("validated source fence")
            .source_id()
            != self.source.source_id()
        {
            return Err(NamespaceCommandValidationError::ParticipantSourceMismatch {
                role: NamespaceEndpointRole::Source,
            });
        }
        if source_entry_leaf(self.source.relative_path().as_str())
            == self.destination.leaf().as_str()
        {
            return Err(NamespaceCommandValidationError::NoOp);
        }
        validate_composed_target_path(
            self.destination.parent().locator(),
            self.destination.leaf(),
        )?;
        Ok(())
    }

    fn validate_shape(&self) -> Result<(), NamespaceCommandValidationError> {
        self.source.validate_shape()?;
        self.destination.validate_shape()
    }

    fn source_parent_locator(&self) -> NamespaceDirectoryLocator {
        parent_locator(self.source.relative_path().as_str())
    }
}

/// One file move, or one same-source child-directory move, retaining the source leaf name.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NamespaceMoveIntentArgs {
    source: NamespaceEntryEndpoint,
    destination_parent: NamespaceDirectoryEndpoint,
}

impl NamespaceMoveIntentArgs {
    /// Construct a move observation.  The destination leaf is always the source leaf, matching
    /// the native folder-browser move request; a move-and-rename is a separate command.
    pub fn new(
        source: NamespaceEntryEndpoint,
        destination_parent: NamespaceDirectoryEndpoint,
    ) -> Result<Self, NamespaceCommandValidationError> {
        let args = Self {
            source,
            destination_parent,
        };
        args.validate_shape()?;
        validate_composed_target_path_str(
            args.destination_parent.locator(),
            source_entry_leaf(args.source.relative_path().as_str()),
        )?;
        Ok(args)
    }

    /// Return the existing source endpoint.
    pub fn source(&self) -> &NamespaceEntryEndpoint {
        &self.source
    }

    /// Return the destination parent endpoint.
    pub fn destination_parent(&self) -> &NamespaceDirectoryEndpoint {
        &self.destination_parent
    }

    /// Validate endpoint relationships against the retained participant observations.
    pub fn validate_against_participants(
        &self,
        participants: &ParticipantSet,
    ) -> Result<(), NamespaceCommandValidationError> {
        participants.validate_for_new_intent()?;
        self.validate_shape()?;
        let source =
            validate_entry_endpoint(participants, NamespaceEndpointRole::Source, &self.source)?;
        let destination = validate_directory_endpoint(
            participants,
            NamespaceEndpointRole::Destination,
            &self.destination_parent,
        )?;
        require_filesystem_participant(participants)?;
        validate_composed_target_path_str(
            self.destination_parent.locator(),
            source_entry_leaf(self.source.relative_path().as_str()),
        )?;

        let source_fence = source.source_fence().expect("validated source fence");
        if same_namespace(
            source_fence.root_identity(),
            destination
                .source_fence()
                .expect("validated source fence")
                .root_identity(),
        ) && same_directory_locator(
            source_fence,
            &self.source_parent_locator(),
            destination.source_fence().expect("validated source fence"),
            self.destination_parent.locator(),
        ) {
            return Err(NamespaceCommandValidationError::NoOp);
        }

        if self.source.expectation().kind() == NamespaceEntryKind::Directory {
            if !same_namespace(
                source_fence.root_identity(),
                destination
                    .source_fence()
                    .expect("validated source fence")
                    .root_identity(),
            ) {
                return Err(NamespaceCommandValidationError::FolderMoveCrossNamespace);
            }
            if source_fence.database_identity()
                != destination
                    .source_fence()
                    .expect("validated source fence")
                    .database_identity()
            {
                return Err(NamespaceCommandValidationError::FolderMoveCrossNamespace);
            }
            if let NamespaceEntryExpectation::Directory(source_expectation) =
                self.source.expectation()
                && source_expectation.filesystem_identity()
                    == self.destination_parent.expectation().filesystem_identity()
            {
                return Err(NamespaceCommandValidationError::FolderMoveIntoItself);
            }
            if let Some(destination_path) = self.destination_parent.locator().child_path() {
                if is_same_or_descendant(
                    self.source.relative_path().as_str(),
                    destination_path.as_str(),
                ) {
                    return Err(NamespaceCommandValidationError::FolderMoveIntoItself);
                }
            }
        }
        Ok(())
    }

    fn validate_shape(&self) -> Result<(), NamespaceCommandValidationError> {
        self.source.validate_shape()?;
        self.destination_parent.validate_shape()
    }

    fn source_parent_locator(&self) -> NamespaceDirectoryLocator {
        parent_locator(self.source.relative_path().as_str())
    }
}

/// One new child directory under a source-root or child-directory parent.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreateFolderIntentArgs {
    parent: NamespaceDirectoryEndpoint,
    leaf: NamespaceLeafName,
}

impl CreateFolderIntentArgs {
    /// Construct a folder-creation observation without touching the filesystem.
    pub fn new(
        parent: NamespaceDirectoryEndpoint,
        leaf: NamespaceLeafName,
    ) -> Result<Self, NamespaceCommandValidationError> {
        let args = Self { parent, leaf };
        args.validate_shape()?;
        validate_composed_target_path(args.parent.locator(), &args.leaf)?;
        Ok(args)
    }

    /// Return the observed parent endpoint.
    pub fn parent(&self) -> &NamespaceDirectoryEndpoint {
        &self.parent
    }

    /// Return the new child leaf.
    pub fn leaf(&self) -> &NamespaceLeafName {
        &self.leaf
    }

    /// Validate the required source and filesystem participants.
    pub fn validate_against_participants(
        &self,
        participants: &ParticipantSet,
    ) -> Result<(), NamespaceCommandValidationError> {
        participants.validate_for_new_intent()?;
        self.validate_shape()?;
        validate_directory_endpoint(
            participants,
            NamespaceEndpointRole::Destination,
            &self.parent,
        )?;
        validate_composed_target_path(self.parent.locator(), &self.leaf)?;
        require_filesystem_participant(participants)
    }

    fn validate_shape(&self) -> Result<(), NamespaceCommandValidationError> {
        self.parent.validate_shape()?;
        self.leaf.validate()
    }
}

/// Namespace command arguments carried by the next universal envelope.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum NamespaceCommandArgs {
    /// Rename one file or child directory within its current parent.
    Rename(NamespaceRenameIntentArgs),
    /// Move one file or one same-source child directory while retaining its leaf name.
    Move(NamespaceMoveIntentArgs),
    /// Create one child directory under a source-root or child-directory parent.
    CreateFolder(CreateFolderIntentArgs),
}

impl NamespaceCommandArgs {
    /// Return the universal command kind represented by these arguments.
    pub const fn command_kind(&self) -> UniversalCommandKind {
        match self {
            Self::Rename(_) => UniversalCommandKind::Rename,
            Self::Move(_) => UniversalCommandKind::Move,
            Self::CreateFolder(_) => UniversalCommandKind::CreateFolder,
        }
    }

    /// Return the only supported namespace collision policy.
    ///
    /// Namespace commands are always no-replace observations. The policy is intentionally
    /// implicit in the command shape so a later execution owner cannot silently reinterpret it.
    pub const fn collision_policy(&self) -> CopyCollisionPolicy {
        CopyCollisionPolicy::NoReplace
    }

    /// Validate command-specific roles, identities, and bounded locators.
    pub fn validate_against_participants(
        &self,
        participants: &ParticipantSet,
    ) -> Result<(), NamespaceCommandValidationError> {
        match self {
            Self::Rename(args) => args.validate_against_participants(participants),
            Self::Move(args) => args.validate_against_participants(participants),
            Self::CreateFolder(args) => args.validate_against_participants(participants),
        }
    }
}

/// Which endpoint role failed participant resolution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NamespaceEndpointRole {
    /// The existing source entry.
    Source,
    /// The destination parent.
    Destination,
}

/// Why a namespace leaf failed its portable bounded contract.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum NamespaceLeafViolation {
    /// The leaf had no bytes.
    #[error("is empty")]
    Empty,
    /// The leaf exceeded its byte bound.
    #[error("exceeds its byte bound")]
    TooLong,
    /// The leaf contained a NUL byte.
    #[error("contains an embedded NUL byte")]
    EmbeddedNul,
    /// The leaf contained a path separator.
    #[error("contains a path separator")]
    Separator,
    /// The leaf was a dot component.
    #[error("is a dot component")]
    DotComponent,
    /// The leaf contained a platform prefix marker.
    #[error("contains a platform prefix")]
    PlatformPrefix,
    /// The leaf contained control text.
    #[error("contains a control character")]
    ControlCharacter,
}

/// Failure while constructing or validating a namespace command observation.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum NamespaceCommandValidationError {
    /// A portable locator failed validation.
    #[error("invalid namespace path: {0}")]
    InvalidPath(#[from] StoredRelativePathError),
    /// A source identity failed its bounded text contract.
    #[error("namespace source identity is invalid")]
    InvalidSourceIdentity,
    /// A physical entry identity failed its bounded text contract.
    #[error("namespace physical identity is invalid")]
    InvalidPhysicalIdentity,
    /// A destination leaf failed its bounded text contract.
    #[error("invalid namespace destination leaf: {0}")]
    InvalidLeaf(#[source] NamespaceLeafViolation),
    /// The participant descriptor was not present.
    #[error("namespace {role:?} participant is missing")]
    ParticipantMissing {
        /// Endpoint role whose participant was absent.
        role: NamespaceEndpointRole,
    },
    /// The participant was not a source database.
    #[error("namespace {role:?} participant is not a required source database")]
    ParticipantWrongKind {
        /// Endpoint role whose participant had another kind.
        role: NamespaceEndpointRole,
    },
    /// The participant was optional rather than required.
    #[error("namespace {role:?} participant is not required")]
    ParticipantNotRequired {
        /// Endpoint role whose participant was optional.
        role: NamespaceEndpointRole,
    },
    /// The source participant did not carry an ownership observation.
    #[error("namespace {role:?} participant is missing its source fence")]
    ParticipantFenceMissing {
        /// Endpoint role whose source fence was absent.
        role: NamespaceEndpointRole,
    },
    /// The endpoint source ID differed from its participant fence.
    #[error("namespace {role:?} endpoint source identity does not match its participant")]
    ParticipantSourceMismatch {
        /// Endpoint role whose source identity mismatched.
        role: NamespaceEndpointRole,
    },
    /// The endpoint expectation was captured at another source revision.
    #[error("namespace {role:?} endpoint source revision does not match its participant")]
    SourceRevisionMismatch {
        /// Endpoint role whose source revision mismatched.
        role: NamespaceEndpointRole,
    },
    /// A root parent expectation did not match the source fence root identity.
    #[error("namespace root parent identity does not match its source fence")]
    RootIdentityMismatch,
    /// The operation omitted the required filesystem participant.
    #[error("namespace command requires a required Filesystem participant")]
    RequiredFilesystemParticipantMissing,
    /// Rename must remain within one exact source participant and parent.
    #[error("namespace rename must use the same required source participant and parent")]
    RenameRequiresSameParent,
    /// The command would leave the namespace unchanged.
    #[error("namespace command is a no-op")]
    NoOp,
    /// A folder move crossed physical source namespaces.
    #[error("folder move must remain within one physical source namespace")]
    FolderMoveCrossNamespace,
    /// A folder move targeted itself or one of its descendants.
    #[error("folder move cannot target itself or a descendant")]
    FolderMoveIntoItself,
    /// The participant set was not a valid initial universal-intent set.
    #[error("invalid namespace command participants: {0}")]
    Participants(#[from] ParticipantValidationError),
}

fn validate_source_id(source_id: &SourceId) -> Result<(), NamespaceCommandValidationError> {
    if source_id.as_str().is_empty()
        || source_id.as_str().len() > super::participants::MAX_SOURCE_IDENTITY_BYTES
        || source_id.as_str().chars().any(char::is_control)
    {
        Err(NamespaceCommandValidationError::InvalidSourceIdentity)
    } else {
        Ok(())
    }
}

fn validate_identity(identity: &str) -> Result<(), NamespaceCommandValidationError> {
    if identity.is_empty()
        || identity.len() > MAX_NAMESPACE_ENTRY_IDENTITY_BYTES
        || identity.chars().any(char::is_control)
    {
        Err(NamespaceCommandValidationError::InvalidPhysicalIdentity)
    } else {
        Ok(())
    }
}

fn validate_entry_endpoint<'a>(
    participants: &'a ParticipantSet,
    role: NamespaceEndpointRole,
    endpoint: &NamespaceEntryEndpoint,
) -> Result<&'a ParticipantDescriptor, NamespaceCommandValidationError> {
    endpoint.validate_shape()?;
    let descriptor = required_source_descriptor(participants, role, endpoint.participant_id())?;
    let fence = descriptor
        .source_fence()
        .ok_or(NamespaceCommandValidationError::ParticipantFenceMissing { role })?;
    if fence.source_id() != endpoint.source_id() {
        return Err(NamespaceCommandValidationError::ParticipantSourceMismatch { role });
    }
    if fence.source_revision() != endpoint.expectation().source_revision() {
        return Err(NamespaceCommandValidationError::SourceRevisionMismatch { role });
    }
    Ok(descriptor)
}

fn validate_directory_endpoint<'a>(
    participants: &'a ParticipantSet,
    role: NamespaceEndpointRole,
    endpoint: &NamespaceDirectoryEndpoint,
) -> Result<&'a ParticipantDescriptor, NamespaceCommandValidationError> {
    endpoint.validate_shape()?;
    let descriptor = required_source_descriptor(participants, role, endpoint.participant_id())?;
    let fence = descriptor
        .source_fence()
        .ok_or(NamespaceCommandValidationError::ParticipantFenceMissing { role })?;
    if fence.source_id() != endpoint.source_id() {
        return Err(NamespaceCommandValidationError::ParticipantSourceMismatch { role });
    }
    if fence.source_revision() != endpoint.expectation().source_revision() {
        return Err(NamespaceCommandValidationError::SourceRevisionMismatch { role });
    }
    if endpoint.locator() == &NamespaceDirectoryLocator::Root
        && endpoint.expectation().filesystem_identity() != fence.root_identity()
    {
        return Err(NamespaceCommandValidationError::RootIdentityMismatch);
    }
    Ok(descriptor)
}

fn required_source_descriptor(
    participants: &ParticipantSet,
    role: NamespaceEndpointRole,
    participant_id: ParticipantId,
) -> Result<&ParticipantDescriptor, NamespaceCommandValidationError> {
    let descriptor = participants
        .descriptors()
        .iter()
        .find(|descriptor| descriptor.id() == participant_id)
        .ok_or(NamespaceCommandValidationError::ParticipantMissing { role })?;
    if descriptor.kind() != ParticipantKind::SourceDatabase {
        return Err(NamespaceCommandValidationError::ParticipantWrongKind { role });
    }
    if descriptor.requirement() != ParticipantRequirement::Required {
        return Err(NamespaceCommandValidationError::ParticipantNotRequired { role });
    }
    if descriptor.source_fence().is_none() {
        return Err(NamespaceCommandValidationError::ParticipantFenceMissing { role });
    }
    Ok(descriptor)
}

fn require_filesystem_participant(
    participants: &ParticipantSet,
) -> Result<(), NamespaceCommandValidationError> {
    if participants.descriptors().iter().any(|descriptor| {
        descriptor.kind() == ParticipantKind::Filesystem
            && descriptor.requirement() == ParticipantRequirement::Required
    }) {
        Ok(())
    } else {
        Err(NamespaceCommandValidationError::RequiredFilesystemParticipantMissing)
    }
}

fn source_entry_leaf(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn parent_locator(path: &str) -> NamespaceDirectoryLocator {
    match path.rsplit_once('/') {
        Some((parent, _)) => NamespaceDirectoryLocator::Child(
            StoredRelativePath::new(parent).expect("validated endpoint parent path"),
        ),
        None => NamespaceDirectoryLocator::Root,
    }
}

fn same_namespace(left_root_identity: &str, right_root_identity: &str) -> bool {
    left_root_identity == right_root_identity
}

fn same_directory_locator(
    left_fence: &super::participants::StoredSourceFence,
    left_locator: &NamespaceDirectoryLocator,
    right_fence: &super::participants::StoredSourceFence,
    right_locator: &NamespaceDirectoryLocator,
) -> bool {
    same_namespace(left_fence.root_identity(), right_fence.root_identity())
        && left_locator == right_locator
}

fn is_same_or_descendant(source_path: &str, candidate_parent: &str) -> bool {
    candidate_parent == source_path
        || candidate_parent
            .strip_prefix(source_path)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn validate_composed_target_path(
    parent: &NamespaceDirectoryLocator,
    leaf: &NamespaceLeafName,
) -> Result<(), NamespaceCommandValidationError> {
    leaf.validate()?;
    validate_composed_target_path_str(parent, leaf.as_str())
}

fn validate_composed_target_path_str(
    parent: &NamespaceDirectoryLocator,
    leaf: &str,
) -> Result<(), NamespaceCommandValidationError> {
    if leaf.is_empty()
        || leaf.len() > MAX_NAMESPACE_LEAF_NAME_BYTES
        || leaf.as_bytes().contains(&0)
        || leaf.contains(['/', '\\', ':'])
        || leaf == "."
        || leaf == ".."
        || leaf.chars().any(char::is_control)
    {
        return Err(NamespaceCommandValidationError::InvalidLeaf(
            if leaf.is_empty() {
                NamespaceLeafViolation::Empty
            } else if leaf.len() > MAX_NAMESPACE_LEAF_NAME_BYTES {
                NamespaceLeafViolation::TooLong
            } else if leaf.as_bytes().contains(&0) {
                NamespaceLeafViolation::EmbeddedNul
            } else if leaf.contains(['/', '\\']) {
                NamespaceLeafViolation::Separator
            } else if leaf == "." || leaf == ".." {
                NamespaceLeafViolation::DotComponent
            } else if leaf.contains(':') {
                NamespaceLeafViolation::PlatformPrefix
            } else {
                NamespaceLeafViolation::ControlCharacter
            },
        ));
    }
    let parent_bytes = parent.child_path().map_or(0, |path| path.as_str().len());
    let separator_bytes = usize::from(parent.child_path().is_some());
    if parent_bytes
        .saturating_add(separator_bytes)
        .saturating_add(leaf.len())
        > super::coordinator::MAX_STORED_RELATIVE_PATH_BYTES
    {
        return Err(NamespaceCommandValidationError::InvalidPath(
            StoredRelativePathError::TooLong,
        ));
    }
    let parent_components = parent
        .child_path()
        .map_or(0, |path| path.as_str().split('/').count());
    if parent_components.saturating_add(1) > super::coordinator::MAX_STORED_RELATIVE_PATH_COMPONENTS
    {
        return Err(NamespaceCommandValidationError::InvalidPath(
            StoredRelativePathError::TooManyComponents,
        ));
    }
    Ok(())
}

/// Validate the tagged namespace command value before deserializing it into owned fields.
///
/// The record codec has already bounded the complete JSON value. This borrowed preflight keeps
/// malformed or oversized nested strings from being cloned into a command object and preserves
/// strict unknown-field rejection for the V8 wire boundary.
pub(crate) fn validate_persisted_namespace_command_args_value(
    value: &Value,
) -> Result<(), &'static str> {
    let object = value
        .as_object()
        .ok_or("persisted namespace command must be a tagged object")?;
    if object.len() != 1 {
        return Err("persisted namespace command must contain exactly one variant");
    }
    let (variant, arguments) = object
        .iter()
        .next()
        .ok_or("persisted namespace command must contain exactly one variant")?;
    match variant.as_str() {
        "Rename" => validate_namespace_rename_value(arguments),
        "Move" => validate_namespace_move_value(arguments),
        "CreateFolder" => validate_namespace_create_folder_value(arguments),
        _ => Err("persisted namespace command contains an unknown variant"),
    }
}

fn validate_namespace_rename_value(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(value, &["source", "destination"])?;
    let source = required_field(object, "source")?;
    let destination = required_field(object, "destination")?;
    validate_persisted_namespace_entry_endpoint_value(source)?;
    validate_namespace_target_value(destination)
}

fn validate_namespace_move_value(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(value, &["source", "destination_parent"])?;
    validate_persisted_namespace_entry_endpoint_value(required_field(object, "source")?)?;
    validate_namespace_directory_endpoint_value(required_field(object, "destination_parent")?)
}

fn validate_namespace_create_folder_value(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(value, &["parent", "leaf"])?;
    validate_namespace_directory_endpoint_value(required_field(object, "parent")?)?;
    validate_namespace_leaf_value(required_field(object, "leaf")?)
}

pub(crate) fn validate_persisted_namespace_entry_endpoint_value(
    value: &Value,
) -> Result<(), &'static str> {
    let object = strict_object(
        value,
        &[
            "participant_id",
            "source_id",
            "relative_path",
            "expectation",
        ],
    )?;
    validate_namespace_participant_id(required_field(object, "participant_id")?)?;
    validate_namespace_source_id(required_field(object, "source_id")?)?;
    validate_namespace_relative_path(required_field(object, "relative_path")?)?;
    validate_namespace_entry_expectation_value(required_field(object, "expectation")?)
}

fn validate_namespace_directory_endpoint_value(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(
        value,
        &["participant_id", "source_id", "locator", "expectation"],
    )?;
    validate_namespace_participant_id(required_field(object, "participant_id")?)?;
    validate_namespace_source_id(required_field(object, "source_id")?)?;
    validate_namespace_locator_value(required_field(object, "locator")?)?;
    validate_namespace_directory_expectation_value(required_field(object, "expectation")?)
}

fn validate_namespace_target_value(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(value, &["parent", "leaf"])?;
    validate_namespace_directory_endpoint_value(required_field(object, "parent")?)?;
    validate_namespace_leaf_value(required_field(object, "leaf")?)
}

fn validate_namespace_entry_expectation_value(value: &Value) -> Result<(), &'static str> {
    let object = value
        .as_object()
        .ok_or("namespace entry expectation must be a tagged object")?;
    if object.len() != 1 {
        return Err("namespace entry expectation must contain exactly one variant");
    }
    let (variant, value) = object
        .iter()
        .next()
        .ok_or("namespace entry expectation must contain exactly one variant")?;
    match variant.as_str() {
        "RegularFile" => {
            let object = strict_object(
                value,
                &[
                    "source_revision",
                    "filesystem_identity",
                    "byte_len",
                    "modified_unix_ns",
                ],
            )?;
            validate_namespace_u64(required_field(object, "source_revision")?)?;
            validate_namespace_identity_value(required_field(object, "filesystem_identity")?)?;
            validate_namespace_u64(required_field(object, "byte_len")?)?;
            validate_namespace_optional_i64(required_field(object, "modified_unix_ns")?)
        }
        "Directory" => validate_namespace_directory_expectation_value(value),
        _ => Err("namespace entry expectation contains an unknown variant"),
    }
}

fn validate_namespace_directory_expectation_value(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(
        value,
        &["source_revision", "filesystem_identity", "modified_unix_ns"],
    )?;
    validate_namespace_u64(required_field(object, "source_revision")?)?;
    validate_namespace_identity_value(required_field(object, "filesystem_identity")?)?;
    validate_namespace_optional_i64(required_field(object, "modified_unix_ns")?)
}

fn validate_namespace_locator_value(value: &Value) -> Result<(), &'static str> {
    if value.as_str() == Some("Root") {
        return Ok(());
    }
    let object = strict_object(value, &["Child"])?;
    let child = required_field(object, "Child")?
        .as_str()
        .ok_or("namespace child locator must be a string")?;
    validate_namespace_relative_path_text(child)
}

fn validate_namespace_participant_id(value: &Value) -> Result<(), &'static str> {
    if value
        .as_str()
        .and_then(|value| uuid::Uuid::parse_str(value).ok())
        .is_some()
    {
        Ok(())
    } else {
        Err("namespace participant ID must be a UUID string")
    }
}

fn validate_namespace_source_id(value: &Value) -> Result<(), &'static str> {
    let source_id = value
        .as_str()
        .ok_or("namespace source ID must be a string")?;
    if source_id.is_empty()
        || source_id.len() > MAX_SOURCE_IDENTITY_BYTES
        || source_id.chars().any(char::is_control)
    {
        Err("namespace source ID is outside its bound")
    } else {
        Ok(())
    }
}

fn validate_namespace_relative_path(value: &Value) -> Result<(), &'static str> {
    let path = value
        .as_str()
        .ok_or("namespace relative path must be a string")?;
    validate_namespace_relative_path_text(path)
}

fn validate_namespace_relative_path_text(path: &str) -> Result<(), &'static str> {
    if path.is_empty() || path.len() > super::coordinator::MAX_STORED_RELATIVE_PATH_BYTES {
        return Err("namespace relative path is outside its bound");
    }
    if path.as_bytes().contains(&0) || path.contains('\\') || path.starts_with('/') {
        return Err("namespace relative path is not portable");
    }
    let mut components = 0usize;
    for component in path.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err("namespace relative path contains an invalid component");
        }
        if component.contains(':') || component.chars().any(char::is_control) {
            return Err("namespace relative path contains an invalid component");
        }
        components = components.saturating_add(1);
        if components > super::coordinator::MAX_STORED_RELATIVE_PATH_COMPONENTS {
            return Err("namespace relative path exceeds its component bound");
        }
    }
    Ok(())
}

fn validate_namespace_identity_value(value: &Value) -> Result<(), &'static str> {
    let identity = value
        .as_str()
        .ok_or("namespace physical identity must be a string")?;
    if identity.is_empty()
        || identity.len() > MAX_NAMESPACE_ENTRY_IDENTITY_BYTES
        || identity.chars().any(char::is_control)
    {
        Err("namespace physical identity is outside its bound")
    } else {
        Ok(())
    }
}

fn validate_namespace_leaf_value(value: &Value) -> Result<(), &'static str> {
    let leaf = value
        .as_str()
        .ok_or("namespace destination leaf must be a string")?;
    if leaf.is_empty() || leaf.len() > MAX_NAMESPACE_LEAF_NAME_BYTES {
        return Err("namespace destination leaf is outside its bound");
    }
    if leaf.as_bytes().contains(&0)
        || leaf.contains(['/', '\\', ':'])
        || leaf == "."
        || leaf == ".."
        || leaf.chars().any(char::is_control)
    {
        return Err("namespace destination leaf is not portable");
    }
    Ok(())
}

fn validate_namespace_u64(value: &Value) -> Result<(), &'static str> {
    value
        .as_u64()
        .map(|_| ())
        .ok_or("namespace value must be an unsigned integer")
}

fn validate_namespace_optional_i64(value: &Value) -> Result<(), &'static str> {
    if value.is_null() || value.as_i64().is_some() {
        Ok(())
    } else {
        Err("namespace timestamp must be a signed integer or null")
    }
}

fn strict_object<'a>(
    value: &'a Value,
    allowed_fields: &[&str],
) -> Result<&'a serde_json::Map<String, Value>, &'static str> {
    let object = value
        .as_object()
        .ok_or("persisted namespace value must be a JSON object")?;
    if object
        .keys()
        .any(|field| !allowed_fields.contains(&field.as_str()))
    {
        return Err("persisted namespace object contains an unknown field");
    }
    Ok(object)
}

fn required_field<'a>(
    object: &'a serde_json::Map<String, Value>,
    field: &str,
) -> Result<&'a Value, &'static str> {
    object
        .get(field)
        .ok_or("persisted namespace object is missing a required field")
}

#[cfg(test)]
mod tests {
    use super::super::participants::{ParticipantCheckpoint, ParticipantCheckpointState};
    use super::*;

    fn participant_fixture() -> (ParticipantSet, ParticipantId, SourceId) {
        let source_id = SourceId::from_string("source-a");
        let source_participant = ParticipantId::new();
        let filesystem_participant = ParticipantId::new();
        let fence = super::super::participants::StoredSourceFence::new(
            source_id.clone(),
            "root-a",
            "database-a",
            1,
            7,
            1,
        )
        .expect("source fence");
        let participants = ParticipantSet::new(
            vec![
                ParticipantDescriptor::new(
                    source_participant,
                    ParticipantKind::SourceDatabase,
                    ParticipantRequirement::Required,
                    Some(fence),
                    None,
                )
                .expect("source participant"),
                ParticipantDescriptor::new(
                    filesystem_participant,
                    ParticipantKind::Filesystem,
                    ParticipantRequirement::Required,
                    None,
                    None,
                )
                .expect("filesystem participant"),
            ],
            vec![
                ParticipantCheckpoint::new(
                    source_participant,
                    ParticipantCheckpointState::Pending,
                    None,
                    None,
                )
                .expect("source checkpoint"),
                ParticipantCheckpoint::new(
                    filesystem_participant,
                    ParticipantCheckpointState::Pending,
                    None,
                    None,
                )
                .expect("filesystem checkpoint"),
            ],
        )
        .expect("participants");
        (participants, source_participant, source_id)
    }

    fn two_source_participant_fixture(
        second_root_identity: &str,
        second_database_identity: &str,
    ) -> (
        ParticipantSet,
        ParticipantId,
        SourceId,
        ParticipantId,
        SourceId,
    ) {
        let source_a_id = SourceId::from_string("source-a");
        let source_b_id = SourceId::from_string("source-b");
        let source_a_participant = ParticipantId::new();
        let source_b_participant = ParticipantId::new();
        let filesystem_participant = ParticipantId::new();
        let source_a_fence = super::super::participants::StoredSourceFence::new(
            source_a_id.clone(),
            "root-a",
            "database-a",
            1,
            7,
            1,
        )
        .expect("source A fence");
        let source_b_fence = super::super::participants::StoredSourceFence::new(
            source_b_id.clone(),
            second_root_identity,
            second_database_identity,
            1,
            7,
            1,
        )
        .expect("source B fence");
        let participants = ParticipantSet::new(
            vec![
                ParticipantDescriptor::new(
                    source_a_participant,
                    ParticipantKind::SourceDatabase,
                    ParticipantRequirement::Required,
                    Some(source_a_fence),
                    None,
                )
                .expect("source A participant"),
                ParticipantDescriptor::new(
                    source_b_participant,
                    ParticipantKind::SourceDatabase,
                    ParticipantRequirement::Required,
                    Some(source_b_fence),
                    None,
                )
                .expect("source B participant"),
                ParticipantDescriptor::new(
                    filesystem_participant,
                    ParticipantKind::Filesystem,
                    ParticipantRequirement::Required,
                    None,
                    None,
                )
                .expect("filesystem participant"),
            ],
            vec![
                ParticipantCheckpoint::new(
                    source_a_participant,
                    ParticipantCheckpointState::Pending,
                    None,
                    None,
                )
                .expect("source A checkpoint"),
                ParticipantCheckpoint::new(
                    source_b_participant,
                    ParticipantCheckpointState::Pending,
                    None,
                    None,
                )
                .expect("source B checkpoint"),
                ParticipantCheckpoint::new(
                    filesystem_participant,
                    ParticipantCheckpointState::Pending,
                    None,
                    None,
                )
                .expect("filesystem checkpoint"),
            ],
        )
        .expect("two-source participants");
        (
            participants,
            source_a_participant,
            source_a_id,
            source_b_participant,
            source_b_id,
        )
    }

    fn root_parent(
        participant_id: ParticipantId,
        source_id: SourceId,
    ) -> NamespaceDirectoryEndpoint {
        NamespaceDirectoryEndpoint::new(
            participant_id,
            source_id,
            NamespaceDirectoryLocator::Root,
            NamespaceDirectoryExpectation::new(7, "root-a", Some(-1)).expect("root expectation"),
        )
        .expect("root parent")
    }

    fn file_source(participant_id: ParticipantId, source_id: SourceId) -> NamespaceEntryEndpoint {
        NamespaceEntryEndpoint::new(
            participant_id,
            source_id,
            StoredRelativePath::new("old.wav").expect("source path"),
            NamespaceEntryExpectation::RegularFile(
                NamespaceFileExpectation::new(7, "file-a", 12, Some(-2)).expect("file expectation"),
            ),
        )
        .expect("file source")
    }

    #[test]
    fn leaf_rejects_paths_and_platform_forms() {
        for value in ["", ".", "..", "a/b", "a\\b", "C:file", "a\0b"] {
            assert!(NamespaceLeafName::new(value).is_err(), "{value:?}");
        }
        assert_eq!(
            NamespaceLeafName::new("new.wav").unwrap().as_str(),
            "new.wav"
        );
    }

    #[test]
    fn root_parent_is_explicit_and_child_paths_remain_nonempty() {
        assert_eq!(NamespaceDirectoryLocator::root().child_path(), None);
        let child = StoredRelativePath::new("drums").unwrap();
        assert_eq!(
            NamespaceDirectoryLocator::child(child.clone())
                .unwrap()
                .child_path(),
            Some(&child)
        );
        assert!(StoredRelativePath::new("").is_err());
    }

    #[test]
    fn namespace_commands_validate_roles_and_preserve_no_replace_policy() {
        let (participants, source_participant, source_id) = participant_fixture();
        let parent = root_parent(source_participant, source_id.clone());
        let rename = NamespaceCommandArgs::Rename(
            NamespaceRenameIntentArgs::new(
                file_source(source_participant, source_id.clone()),
                NamespaceTarget::new(
                    parent.clone(),
                    NamespaceLeafName::new("new.wav").expect("destination leaf"),
                )
                .expect("destination"),
            )
            .expect("rename"),
        );
        rename
            .validate_against_participants(&participants)
            .expect("valid rename");
        assert_eq!(rename.collision_policy(), CopyCollisionPolicy::NoReplace);

        let create = NamespaceCommandArgs::CreateFolder(
            CreateFolderIntentArgs::new(
                parent,
                NamespaceLeafName::new("new-folder").expect("folder leaf"),
            )
            .expect("create-folder"),
        );
        create
            .validate_against_participants(&participants)
            .expect("valid folder creation");
    }

    #[test]
    fn folder_moves_reject_identity_aliases_and_descendants() {
        let (participants, source_participant, source_id) = participant_fixture();
        let source = NamespaceEntryEndpoint::new(
            source_participant,
            source_id.clone(),
            StoredRelativePath::new("folder").expect("folder path"),
            NamespaceEntryExpectation::Directory(
                NamespaceDirectoryExpectation::new(7, "folder-identity", None)
                    .expect("folder expectation"),
            ),
        )
        .expect("folder source");

        let alias_parent = NamespaceDirectoryEndpoint::new(
            source_participant,
            source_id.clone(),
            NamespaceDirectoryLocator::child(StoredRelativePath::new("alias").unwrap()).unwrap(),
            NamespaceDirectoryExpectation::new(7, "folder-identity", None)
                .expect("alias expectation"),
        )
        .expect("alias parent");
        let alias_move =
            NamespaceMoveIntentArgs::new(source.clone(), alias_parent).expect("alias move shape");
        assert_eq!(
            alias_move.validate_against_participants(&participants),
            Err(NamespaceCommandValidationError::FolderMoveIntoItself)
        );

        let descendant_parent = NamespaceDirectoryEndpoint::new(
            source_participant,
            source_id,
            NamespaceDirectoryLocator::child(StoredRelativePath::new("folder/child").unwrap())
                .unwrap(),
            NamespaceDirectoryExpectation::new(7, "child-identity", None)
                .expect("child expectation"),
        )
        .expect("descendant parent");
        let descendant_move =
            NamespaceMoveIntentArgs::new(source, descendant_parent).expect("descendant move shape");
        assert_eq!(
            descendant_move.validate_against_participants(&participants),
            Err(NamespaceCommandValidationError::FolderMoveIntoItself)
        );
    }

    #[test]
    fn file_moves_can_cross_sources_but_folder_moves_require_one_database_namespace() {
        let (participants, source_a_participant, source_a_id, source_b_participant, source_b_id) =
            two_source_participant_fixture("root-b", "database-b");
        let file_move = NamespaceMoveIntentArgs::new(
            file_source(source_a_participant, source_a_id.clone()),
            NamespaceDirectoryEndpoint::new(
                source_b_participant,
                source_b_id.clone(),
                NamespaceDirectoryLocator::Root,
                NamespaceDirectoryExpectation::new(7, "root-b", None).expect("source B root"),
            )
            .expect("source B parent"),
        )
        .expect("cross-source file move shape");
        file_move
            .validate_against_participants(&participants)
            .expect("regular files may move across sources");

        let folder_source = NamespaceEntryEndpoint::new(
            source_a_participant,
            source_a_id.clone(),
            StoredRelativePath::new("folder").expect("folder path"),
            NamespaceEntryExpectation::Directory(
                NamespaceDirectoryExpectation::new(7, "folder-identity", None)
                    .expect("folder expectation"),
            ),
        )
        .expect("folder source");
        let cross_root = NamespaceMoveIntentArgs::new(
            folder_source.clone(),
            NamespaceDirectoryEndpoint::new(
                source_b_participant,
                source_b_id.clone(),
                NamespaceDirectoryLocator::Child(
                    StoredRelativePath::new("archive").expect("archive path"),
                ),
                NamespaceDirectoryExpectation::new(7, "archive", None)
                    .expect("archive expectation"),
            )
            .expect("cross-root parent"),
        )
        .expect("cross-root folder move shape");
        assert_eq!(
            cross_root.validate_against_participants(&participants),
            Err(NamespaceCommandValidationError::FolderMoveCrossNamespace)
        );

        let (
            same_root_participants,
            same_root_source_a,
            same_root_id_a,
            same_root_source_b,
            same_root_id_b,
        ) = two_source_participant_fixture("root-a", "database-b");
        let different_database = NamespaceMoveIntentArgs::new(
            NamespaceEntryEndpoint::new(
                same_root_source_a,
                same_root_id_a.clone(),
                StoredRelativePath::new("folder").expect("folder path"),
                NamespaceEntryExpectation::Directory(
                    NamespaceDirectoryExpectation::new(7, "folder-identity", None)
                        .expect("folder expectation"),
                ),
            )
            .expect("folder source"),
            NamespaceDirectoryEndpoint::new(
                same_root_source_b,
                same_root_id_b,
                NamespaceDirectoryLocator::Child(
                    StoredRelativePath::new("archive").expect("archive path"),
                ),
                NamespaceDirectoryExpectation::new(7, "archive", None)
                    .expect("archive expectation"),
            )
            .expect("different-database parent"),
        )
        .expect("different-database folder move shape");
        assert_eq!(
            different_database.validate_against_participants(&same_root_participants),
            Err(NamespaceCommandValidationError::FolderMoveCrossNamespace)
        );
    }

    #[test]
    fn source_aliases_still_reject_file_noops_and_folder_self_or_descendant_moves() {
        let (participants, source_a_participant, source_a_id, source_b_participant, source_b_id) =
            two_source_participant_fixture("root-a", "database-a");
        let aliased_file = NamespaceEntryEndpoint::new(
            source_a_participant,
            source_a_id.clone(),
            StoredRelativePath::new("folder/old.wav").expect("file path"),
            NamespaceEntryExpectation::RegularFile(
                NamespaceFileExpectation::new(7, "file-identity", 12, None)
                    .expect("file expectation"),
            ),
        )
        .expect("aliased file");
        let aliased_parent = NamespaceDirectoryEndpoint::new(
            source_b_participant,
            source_b_id.clone(),
            NamespaceDirectoryLocator::Child(
                StoredRelativePath::new("folder").expect("folder path"),
            ),
            NamespaceDirectoryExpectation::new(7, "folder-identity", None)
                .expect("folder expectation"),
        )
        .expect("aliased parent");
        let aliased_file_move = NamespaceMoveIntentArgs::new(aliased_file, aliased_parent)
            .expect("aliased file move shape");
        assert_eq!(
            aliased_file_move.validate_against_participants(&participants),
            Err(NamespaceCommandValidationError::NoOp)
        );

        let folder_source = NamespaceEntryEndpoint::new(
            source_a_participant,
            source_a_id,
            StoredRelativePath::new("folder").expect("folder path"),
            NamespaceEntryExpectation::Directory(
                NamespaceDirectoryExpectation::new(7, "folder-identity", None)
                    .expect("folder expectation"),
            ),
        )
        .expect("folder source");
        let aliased_self_parent = NamespaceDirectoryEndpoint::new(
            source_b_participant,
            source_b_id.clone(),
            NamespaceDirectoryLocator::Child(StoredRelativePath::new("alias").expect("alias path")),
            NamespaceDirectoryExpectation::new(7, "folder-identity", None)
                .expect("aliased folder identity"),
        )
        .expect("aliased self parent");
        let aliased_self_move =
            NamespaceMoveIntentArgs::new(folder_source.clone(), aliased_self_parent)
                .expect("aliased self move shape");
        assert_eq!(
            aliased_self_move.validate_against_participants(&participants),
            Err(NamespaceCommandValidationError::FolderMoveIntoItself)
        );

        let descendant_parent = NamespaceDirectoryEndpoint::new(
            source_b_participant,
            source_b_id,
            NamespaceDirectoryLocator::Child(
                StoredRelativePath::new("folder/child").expect("descendant path"),
            ),
            NamespaceDirectoryExpectation::new(7, "child-identity", None)
                .expect("descendant expectation"),
        )
        .expect("descendant parent");
        let descendant_move = NamespaceMoveIntentArgs::new(folder_source, descendant_parent)
            .expect("descendant move shape");
        assert_eq!(
            descendant_move.validate_against_participants(&participants),
            Err(NamespaceCommandValidationError::FolderMoveIntoItself)
        );
    }

    #[test]
    fn rename_requires_exact_parent_and_root_identity_and_rejects_same_leaf() {
        let (participants, source_a_participant, source_a_id, source_b_participant, source_b_id) =
            two_source_participant_fixture("root-b", "database-b");
        let source = file_source(source_a_participant, source_a_id.clone());
        let same_leaf = NamespaceRenameIntentArgs::new(
            source.clone(),
            NamespaceTarget::new(
                root_parent(source_a_participant, source_a_id.clone()),
                NamespaceLeafName::new("old.wav").expect("same leaf"),
            )
            .expect("same-leaf target"),
        )
        .expect("same-leaf rename shape");
        assert_eq!(
            same_leaf.validate_against_participants(&participants),
            Err(NamespaceCommandValidationError::NoOp)
        );

        let different_parent = NamespaceRenameIntentArgs::new(
            source.clone(),
            NamespaceTarget::new(
                NamespaceDirectoryEndpoint::new(
                    source_a_participant,
                    source_a_id.clone(),
                    NamespaceDirectoryLocator::Child(
                        StoredRelativePath::new("archive").expect("archive path"),
                    ),
                    NamespaceDirectoryExpectation::new(7, "archive", None)
                        .expect("archive expectation"),
                )
                .expect("different parent"),
                NamespaceLeafName::new("new.wav").expect("new leaf"),
            )
            .expect("different-parent target"),
        )
        .expect("different-parent rename shape");
        assert_eq!(
            different_parent.validate_against_participants(&participants),
            Err(NamespaceCommandValidationError::RenameRequiresSameParent)
        );

        let different_source = NamespaceRenameIntentArgs::new(
            source.clone(),
            NamespaceTarget::new(
                NamespaceDirectoryEndpoint::new(
                    source_b_participant,
                    source_b_id.clone(),
                    NamespaceDirectoryLocator::Root,
                    NamespaceDirectoryExpectation::new(7, "root-b", None).expect("source B root"),
                )
                .expect("source B parent"),
                NamespaceLeafName::new("new.wav").expect("new leaf"),
            )
            .expect("different-source target"),
        )
        .expect("different-source rename shape");
        assert_eq!(
            different_source.validate_against_participants(&participants),
            Err(NamespaceCommandValidationError::RenameRequiresSameParent)
        );

        let wrong_root = NamespaceRenameIntentArgs::new(
            source,
            NamespaceTarget::new(
                NamespaceDirectoryEndpoint::new(
                    source_a_participant,
                    source_a_id,
                    NamespaceDirectoryLocator::Root,
                    NamespaceDirectoryExpectation::new(7, "wrong-root", None)
                        .expect("wrong root observation"),
                )
                .expect("wrong-root parent"),
                NamespaceLeafName::new("new.wav").expect("new leaf"),
            )
            .expect("wrong-root target"),
        )
        .expect("wrong-root rename shape");
        assert_eq!(
            wrong_root.validate_against_participants(&participants),
            Err(NamespaceCommandValidationError::RootIdentityMismatch)
        );
    }

    #[test]
    fn namespace_validation_rejects_malformed_participant_inventory() {
        let (participants, source_participant, source_id) = participant_fixture();
        let command = NamespaceCommandArgs::CreateFolder(
            CreateFolderIntentArgs::new(
                root_parent(source_participant, source_id.clone()),
                NamespaceLeafName::new("new-folder").expect("folder leaf"),
            )
            .expect("create-folder"),
        );

        let mut duplicate_descriptor = serde_json::to_value(&participants).expect("participants");
        let descriptor = duplicate_descriptor["descriptors"][0].clone();
        duplicate_descriptor["descriptors"]
            .as_array_mut()
            .expect("descriptor array")
            .push(descriptor);
        let duplicate_descriptor: ParticipantSet =
            serde_json::from_value(duplicate_descriptor).expect("decoded duplicate descriptors");
        assert!(matches!(
            command.validate_against_participants(&duplicate_descriptor),
            Err(NamespaceCommandValidationError::Participants(
                ParticipantValidationError::DuplicateDescriptorId { .. }
            ))
        ));

        let mut duplicate_checkpoint = serde_json::to_value(&participants).expect("participants");
        let checkpoint = duplicate_checkpoint["checkpoints"][0].clone();
        duplicate_checkpoint["checkpoints"]
            .as_array_mut()
            .expect("checkpoint array")
            .push(checkpoint);
        let duplicate_checkpoint: ParticipantSet =
            serde_json::from_value(duplicate_checkpoint).expect("decoded duplicate checkpoints");
        assert!(matches!(
            command.validate_against_participants(&duplicate_checkpoint),
            Err(NamespaceCommandValidationError::Participants(
                ParticipantValidationError::DuplicateCheckpointId { .. }
            ))
        ));
    }

    #[test]
    fn namespace_paths_validate_composed_target_bounds() {
        let (participants, source_participant, source_id) = participant_fixture();
        let parent_path = "a".repeat(super::super::coordinator::MAX_STORED_RELATIVE_PATH_BYTES);
        let parent = NamespaceDirectoryEndpoint::new(
            source_participant,
            source_id,
            NamespaceDirectoryLocator::child(StoredRelativePath::new(parent_path).unwrap())
                .unwrap(),
            NamespaceDirectoryExpectation::new(7, "parent", None).expect("parent expectation"),
        )
        .expect("parent");
        let command =
            CreateFolderIntentArgs::new(parent, NamespaceLeafName::new("child").expect("child"));
        assert!(matches!(
            command,
            Err(NamespaceCommandValidationError::InvalidPath(
                StoredRelativePathError::TooLong
            ))
        ));
        let _ = participants;
    }
}
