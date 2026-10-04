//! Strict, storage-neutral coordinator command observations.
//!
//! The coordinator command observations are deliberately small: regular-file copies and
//! same-source duplicates between source-database participants, plus one external regular-file
//! import. These values contain source identities, revisions, and bounded relative locators only.
//! They are observations for a later owner; they do not contain a live source lease, filesystem
//! capability, capacity reservation, or permission to execute a command.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

use super::decoded_edit_commands::DecodedEditIntentArgs;
use super::external_commands::{
    ExternalReconciliationIntentArgs, ExternalReconciliationValidationError,
};
use super::namespace_commands::{NamespaceCommandArgs, NamespaceCommandValidationError};
use super::participants::{
    ParticipantId, ParticipantKind, ParticipantRequirement, ParticipantSet,
    ParticipantValidationError,
};
use crate::sample_sources::SourceId;

/// Maximum UTF-8 byte length of a portable stored relative path.
pub const MAX_STORED_RELATIVE_PATH_BYTES: usize = 4096;
/// Maximum number of normal components in a portable stored relative path.
pub const MAX_STORED_RELATIVE_PATH_COMPONENTS: usize = 64;
/// Maximum byte length of an expected source file identity.
pub const MAX_COPY_CONTENT_IDENTITY_BYTES: usize = 256;
/// Maximum byte length of a metadata reference inherited by a copy.
pub const MAX_COPY_METADATA_REFERENCE_BYTES: usize = 1024;
/// Maximum byte length of an observed external root identity.
pub const MAX_IMPORT_ROOT_IDENTITY_BYTES: usize = 256;
/// Maximum byte length of an opaque durable retry lease identifier.
pub const MAX_RETRY_LEASE_ID_BYTES: usize = 128;
/// Maximum byte length of redaction-safe retry context.
pub const MAX_RETRY_CONTEXT_BYTES: usize = 512;

/// A portable, bounded path relative to one source root.
///
/// The wire spelling uses `/` separators on every platform. Empty components, dot components,
/// parent traversal, backslashes, NULs, absolute paths, and platform prefixes are rejected before
/// the value can enter a persisted coordinator envelope.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct StoredRelativePath(String);

impl StoredRelativePath {
    /// Construct a portable relative path after applying its byte and component bounds.
    pub fn new(value: impl Into<String>) -> Result<Self, StoredRelativePathError> {
        let path = Self(value.into());
        path.validate()?;
        Ok(path)
    }

    /// Borrow the canonical portable path spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Validate a value that may have come from an untrusted decoder.
    pub fn validate(&self) -> Result<(), StoredRelativePathError> {
        if self.0.is_empty() {
            return Err(StoredRelativePathError::Empty);
        }
        if self.0.len() > MAX_STORED_RELATIVE_PATH_BYTES {
            return Err(StoredRelativePathError::TooLong);
        }
        if self.0.as_bytes().contains(&0) {
            return Err(StoredRelativePathError::EmbeddedNul);
        }
        if self.0.contains('\\') {
            return Err(StoredRelativePathError::Backslash);
        }
        if self.0.starts_with('/') {
            return Err(StoredRelativePathError::Absolute);
        }

        let mut components = 0usize;
        for component in self.0.split('/') {
            if component.is_empty() {
                return Err(StoredRelativePathError::EmptyComponent);
            }
            if component == "." {
                return Err(StoredRelativePathError::DotComponent);
            }
            if component == ".." {
                return Err(StoredRelativePathError::ParentTraversal);
            }
            if component.contains(':') {
                return Err(StoredRelativePathError::PlatformPrefix);
            }
            if component.chars().any(char::is_control) {
                return Err(StoredRelativePathError::ControlCharacter);
            }
            components = components.saturating_add(1);
            if components > MAX_STORED_RELATIVE_PATH_COMPONENTS {
                return Err(StoredRelativePathError::TooManyComponents);
            }
        }
        Ok(())
    }
}

/// Why a stored relative path was rejected.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum StoredRelativePathError {
    /// The path has no components.
    #[error("stored relative path is empty")]
    Empty,
    /// The path exceeds [`MAX_STORED_RELATIVE_PATH_BYTES`].
    #[error("stored relative path exceeds its byte bound")]
    TooLong,
    /// The path contains an embedded NUL byte.
    #[error("stored relative path contains an embedded NUL byte")]
    EmbeddedNul,
    /// The path uses a backslash and is therefore not portable.
    #[error("stored relative path contains a backslash")]
    Backslash,
    /// The path starts at a root rather than being relative.
    #[error("stored relative path is absolute")]
    Absolute,
    /// Two separators created an empty component.
    #[error("stored relative path contains an empty component")]
    EmptyComponent,
    /// A dot component is not a stable stored locator.
    #[error("stored relative path contains a dot component")]
    DotComponent,
    /// A parent component could escape the source root.
    #[error("stored relative path contains parent traversal")]
    ParentTraversal,
    /// A drive, UNC, or other platform prefix was supplied.
    #[error("stored relative path contains a platform prefix")]
    PlatformPrefix,
    /// A component contains a control character.
    #[error("stored relative path contains a control character")]
    ControlCharacter,
    /// The path exceeds [`MAX_STORED_RELATIVE_PATH_COMPONENTS`].
    #[error("stored relative path exceeds its component bound")]
    TooManyComponents,
}

/// Whether a copy endpoint identifies the source or destination participant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CopyEndpointRole {
    /// The regular file read by the copy owner.
    Source,
    /// The regular file created by the copy owner.
    Destination,
}

impl std::fmt::Display for CopyEndpointRole {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Source => "source",
            Self::Destination => "destination",
        })
    }
}

/// A source-database participant and portable locator used by a copy command.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CopyEndpoint {
    participant_id: ParticipantId,
    source_id: SourceId,
    relative_path: StoredRelativePath,
}

impl CopyEndpoint {
    /// Construct a source-database endpoint observation.
    pub fn new(
        participant_id: ParticipantId,
        source_id: SourceId,
        relative_path: StoredRelativePath,
    ) -> Result<Self, CopyIntentValidationError> {
        validate_source_id(&source_id)?;
        relative_path
            .validate()
            .map_err(CopyIntentValidationError::InvalidPath)?;
        Ok(Self {
            participant_id,
            source_id,
            relative_path,
        })
    }

    /// Return the participant this endpoint must resolve through.
    pub const fn participant_id(&self) -> ParticipantId {
        self.participant_id
    }

    /// Return the configured source identity observation.
    pub fn source_id(&self) -> &SourceId {
        &self.source_id
    }

    /// Return the portable source-relative locator.
    pub fn relative_path(&self) -> &StoredRelativePath {
        &self.relative_path
    }
}

/// Expected regular-file identity and source-revision observation for a copy.
///
/// The file identity is intentionally opaque and follows the source manifest's stable identity
/// convention. It is checked against the live source later and is never a capability or a proof
/// that a file currently exists.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CopySourceExpectation {
    source_revision: u64,
    file_identity: String,
    file_size: u64,
    modified_ns: i64,
}

impl CopySourceExpectation {
    /// Construct a bounded regular-file source observation.
    pub fn new(
        source_revision: u64,
        file_identity: impl Into<String>,
        file_size: u64,
        modified_ns: i64,
    ) -> Result<Self, CopyIntentValidationError> {
        let expectation = Self {
            source_revision,
            file_identity: file_identity.into(),
            file_size,
            modified_ns,
        };
        expectation.validate()?;
        Ok(expectation)
    }

    /// Validate a source observation recovered from storage.
    pub fn validate(&self) -> Result<(), CopyIntentValidationError> {
        validate_file_identity(&self.file_identity)?;
        if self.modified_ns < 0 {
            return Err(CopyIntentValidationError::NegativeModifiedTimestamp);
        }
        Ok(())
    }

    /// Return the source manifest revision observed by the command creator.
    pub const fn source_revision(&self) -> u64 {
        self.source_revision
    }

    /// Return the opaque expected file identity.
    pub fn file_identity(&self) -> &str {
        &self.file_identity
    }

    /// Return the expected regular-file length.
    pub const fn file_size(&self) -> u64 {
        self.file_size
    }

    /// Return the expected modification timestamp in epoch nanoseconds.
    pub const fn modified_ns(&self) -> i64 {
        self.modified_ns
    }
}

/// Collision policy supported by the initial copy command contract.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum CopyCollisionPolicy {
    /// Fail closed if the destination entry already exists.
    NoReplace,
}

/// Typed bounded arguments for a regular-file source-to-source copy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CopyIntentArgs {
    source: CopyEndpoint,
    destination: CopyEndpoint,
    expected_source: CopySourceExpectation,
    collision_policy: CopyCollisionPolicy,
    inherited_metadata_reference: Option<String>,
}

impl CopyIntentArgs {
    /// Construct copy arguments without acquiring any source or filesystem capability.
    pub fn new(
        source: CopyEndpoint,
        destination: CopyEndpoint,
        expected_source: CopySourceExpectation,
        collision_policy: CopyCollisionPolicy,
        inherited_metadata_reference: Option<String>,
    ) -> Result<Self, CopyIntentValidationError> {
        let args = Self {
            source,
            destination,
            expected_source,
            collision_policy,
            inherited_metadata_reference,
        };
        args.validate_shape()?;
        Ok(args)
    }

    /// Validate shape and resolve both endpoint references against stored participants.
    pub fn validate_against_participants(
        &self,
        participants: &ParticipantSet,
    ) -> Result<(), CopyIntentValidationError> {
        self.validate_shape()?;
        participants.validate_for_new_intent()?;

        let source = Self::validate_endpoint(participants, CopyEndpointRole::Source, &self.source)?;
        Self::validate_endpoint(
            participants,
            CopyEndpointRole::Destination,
            &self.destination,
        )?;

        if self.source.source_id == self.destination.source_id
            && self.source.relative_path == self.destination.relative_path
        {
            return Err(CopyIntentValidationError::IdenticalEndpoints);
        }
        if self.expected_source.source_revision()
            != source
                .source_fence()
                .expect("source participant validation checked its fence")
                .source_revision()
        {
            return Err(CopyIntentValidationError::SourceRevisionMismatch);
        }
        if !participants.descriptors().iter().any(|descriptor| {
            descriptor.kind() == ParticipantKind::Filesystem
                && descriptor.requirement() == ParticipantRequirement::Required
        }) {
            return Err(CopyIntentValidationError::RequiredFilesystemParticipantMissing);
        }
        Ok(())
    }

    /// Validate this copy as a same-source duplicate operation.
    ///
    /// DuplicateSame is intentionally narrower than a general copy: both endpoints must resolve
    /// through the same required source participant and source identity, and their locators must
    /// share a parent while naming different destination leaves.
    pub fn validate_as_duplicate_same_against_participants(
        &self,
        participants: &ParticipantSet,
    ) -> Result<(), CopyIntentValidationError> {
        self.validate_against_participants(participants)?;
        if self.source.participant_id != self.destination.participant_id {
            return Err(CopyIntentValidationError::DuplicateSourceParticipantMismatch);
        }
        if self.source.source_id != self.destination.source_id {
            return Err(CopyIntentValidationError::DuplicateSourceIdentityMismatch);
        }
        if relative_parent(self.source.relative_path.as_str())
            != relative_parent(self.destination.relative_path.as_str())
        {
            return Err(CopyIntentValidationError::DuplicateParentDirectoryMismatch);
        }
        if relative_leaf(self.source.relative_path.as_str())
            == relative_leaf(self.destination.relative_path.as_str())
        {
            return Err(CopyIntentValidationError::DuplicateDestinationLeafMismatch);
        }
        Ok(())
    }

    /// Return the source endpoint observation.
    pub fn source(&self) -> &CopyEndpoint {
        &self.source
    }

    /// Return the destination endpoint observation.
    pub fn destination(&self) -> &CopyEndpoint {
        &self.destination
    }

    /// Return the expected source observation.
    pub fn expected_source(&self) -> &CopySourceExpectation {
        &self.expected_source
    }

    /// Return the explicit no-replace policy.
    pub const fn collision_policy(&self) -> CopyCollisionPolicy {
        self.collision_policy
    }

    /// Return the bounded inherited metadata reference, when one was supplied.
    pub fn inherited_metadata_reference(&self) -> Option<&str> {
        self.inherited_metadata_reference.as_deref()
    }

    fn validate_shape(&self) -> Result<(), CopyIntentValidationError> {
        self.source
            .relative_path
            .validate()
            .map_err(CopyIntentValidationError::InvalidPath)?;
        self.destination
            .relative_path
            .validate()
            .map_err(CopyIntentValidationError::InvalidPath)?;
        validate_source_id(&self.source.source_id)?;
        validate_source_id(&self.destination.source_id)?;
        self.expected_source.validate()?;
        if let Some(reference) = &self.inherited_metadata_reference {
            validate_bounded_text(reference, MAX_COPY_METADATA_REFERENCE_BYTES)
                .map_err(|_| CopyIntentValidationError::InvalidMetadataReference)?;
        }
        Ok(())
    }

    pub(crate) fn validate_endpoint<'a>(
        participants: &'a ParticipantSet,
        role: CopyEndpointRole,
        endpoint: &CopyEndpoint,
    ) -> Result<&'a super::participants::ParticipantDescriptor, CopyIntentValidationError> {
        let descriptor = participants
            .descriptors()
            .iter()
            .find(|descriptor| descriptor.id() == endpoint.participant_id())
            .ok_or(CopyIntentValidationError::ParticipantMissing {
                role,
                participant_id: endpoint.participant_id(),
            })?;
        if descriptor.kind() != ParticipantKind::SourceDatabase {
            return Err(CopyIntentValidationError::ParticipantWrongKind {
                role,
                actual: descriptor.kind(),
            });
        }
        if descriptor.requirement() != ParticipantRequirement::Required {
            return Err(CopyIntentValidationError::ParticipantNotRequired { role });
        }
        let fence = descriptor
            .source_fence()
            .ok_or(CopyIntentValidationError::ParticipantFenceMissing { role })?;
        if fence.source_id() != endpoint.source_id() {
            return Err(CopyIntentValidationError::ParticipantSourceMismatch { role });
        }
        Ok(descriptor)
    }
}

/// Errors returned while constructing or validating copy arguments.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum CopyIntentValidationError {
    /// A portable locator failed its path contract.
    #[error("invalid copy endpoint path: {0}")]
    InvalidPath(#[from] StoredRelativePathError),
    /// A source identifier failed its bounded text contract.
    #[error("copy endpoint source identity is invalid")]
    InvalidSourceIdentity,
    /// The expected file identity was empty, oversized, or contained control text.
    #[error("copy expected file identity is invalid")]
    InvalidFileIdentity,
    /// The inherited metadata reference was outside its bounded text contract.
    #[error("copy inherited metadata reference is invalid")]
    InvalidMetadataReference,
    /// Modification timestamps cannot precede the Unix epoch.
    #[error("copy expected modification timestamp must be non-negative")]
    NegativeModifiedTimestamp,
    /// The source and destination identify the same source-relative entry.
    #[error("copy source and destination endpoints are identical")]
    IdenticalEndpoints,
    /// DuplicateSame endpoints resolved through different participant identities.
    #[error("same-source duplicate endpoints must use the same source participant")]
    DuplicateSourceParticipantMismatch,
    /// DuplicateSame endpoints carried different source identity observations.
    #[error("same-source duplicate endpoints must use the same source identity")]
    DuplicateSourceIdentityMismatch,
    /// DuplicateSame endpoints did not share a parent directory.
    #[error("same-source duplicate endpoints must share a parent directory")]
    DuplicateParentDirectoryMismatch,
    /// DuplicateSame endpoints did not name distinct leaves.
    #[error("same-source duplicate endpoints must name distinct destination leaves")]
    DuplicateDestinationLeafMismatch,
    /// An endpoint participant was not in the participant set.
    #[error("copy {role} participant {participant_id} is missing")]
    ParticipantMissing {
        /// Endpoint role being resolved.
        role: CopyEndpointRole,
        /// Missing participant identity.
        participant_id: ParticipantId,
    },
    /// An endpoint participant was not a source database participant.
    #[error("copy {role} participant has kind {actual:?}, expected SourceDatabase")]
    ParticipantWrongKind {
        /// Endpoint role being resolved.
        role: CopyEndpointRole,
        /// Stored participant kind.
        actual: ParticipantKind,
    },
    /// An endpoint participant was optional rather than required.
    #[error("copy {role} participant is not required")]
    ParticipantNotRequired {
        /// Endpoint role being resolved.
        role: CopyEndpointRole,
    },
    /// An endpoint participant did not carry its required source fence.
    #[error("copy {role} participant has no source fence")]
    ParticipantFenceMissing {
        /// Endpoint role being resolved.
        role: CopyEndpointRole,
    },
    /// The endpoint source identity did not match its participant fence.
    #[error("copy {role} endpoint source identity does not match its participant fence")]
    ParticipantSourceMismatch {
        /// Endpoint role being resolved.
        role: CopyEndpointRole,
    },
    /// The expected source revision did not match the source participant fence.
    #[error("copy expected source revision does not match the source participant fence")]
    SourceRevisionMismatch,
    /// A required filesystem participant was absent.
    #[error("copy requires a required Filesystem participant")]
    RequiredFilesystemParticipantMissing,
    /// Participant shape validation failed.
    #[error("invalid copy participants: {0}")]
    Participants(#[from] ParticipantValidationError),
}

fn relative_parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

fn relative_leaf(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, leaf)| leaf)
}

/// Opaque identity for the external root binding observed by an Import command.
///
/// The UUID identifies a later owner lookup. It is not a path, an open descriptor, or an
/// authorization to read from the external root.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ExternalRootBindingId(Uuid);

impl ExternalRootBindingId {
    /// Generate a new opaque external-root binding identity.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Wrap an existing UUID without interpreting it as live authority.
    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    /// Return the UUID used by the persisted representation.
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for ExternalRootBindingId {
    fn default() -> Self {
        Self::new()
    }
}

/// Expected regular-file facts observed at an external root for an Import command.
///
/// Unlike [`CopySourceExpectation`], this observation has no source catalog revision because an
/// external root is not represented by a source database participant.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImportFileExpectation {
    file_identity: String,
    file_size: u64,
    modified_ns: i64,
}

impl ImportFileExpectation {
    /// Construct bounded external regular-file facts.
    pub fn new(
        file_identity: impl Into<String>,
        file_size: u64,
        modified_ns: i64,
    ) -> Result<Self, ImportIntentValidationError> {
        let expectation = Self {
            file_identity: file_identity.into(),
            file_size,
            modified_ns,
        };
        expectation.validate()?;
        Ok(expectation)
    }

    /// Validate facts recovered from an untrusted V6 record.
    pub fn validate(&self) -> Result<(), ImportIntentValidationError> {
        validate_bounded_text(&self.file_identity, MAX_COPY_CONTENT_IDENTITY_BYTES)
            .map_err(|_| ImportIntentValidationError::InvalidFileIdentity)?;
        if self.modified_ns < 0 {
            return Err(ImportIntentValidationError::NegativeModifiedTimestamp);
        }
        Ok(())
    }

    /// Return the opaque observed regular-file identity.
    pub fn file_identity(&self) -> &str {
        &self.file_identity
    }

    /// Return the observed regular-file length.
    pub const fn file_size(&self) -> u64 {
        self.file_size
    }

    /// Return the observed modification timestamp in epoch nanoseconds.
    pub const fn modified_ns(&self) -> i64 {
        self.modified_ns
    }
}

/// Typed bounded arguments for importing one external regular file into a source database.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImportIntentArgs {
    external_root_binding_id: ExternalRootBindingId,
    filesystem_participant_id: ParticipantId,
    root_identity: String,
    relative_path: StoredRelativePath,
    expected_file: ImportFileExpectation,
    destination: CopyEndpoint,
    collision_policy: CopyCollisionPolicy,
    inherited_metadata_reference: Option<String>,
}

impl ImportIntentArgs {
    /// Construct Import observations without opening or retaining the external root.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        external_root_binding_id: ExternalRootBindingId,
        filesystem_participant_id: ParticipantId,
        root_identity: impl Into<String>,
        relative_path: StoredRelativePath,
        expected_file: ImportFileExpectation,
        destination: CopyEndpoint,
        collision_policy: CopyCollisionPolicy,
        inherited_metadata_reference: Option<String>,
    ) -> Result<Self, ImportIntentValidationError> {
        let args = Self {
            external_root_binding_id,
            filesystem_participant_id,
            root_identity: root_identity.into(),
            relative_path,
            expected_file,
            destination,
            collision_policy,
            inherited_metadata_reference,
        };
        args.validate_shape()?;
        Ok(args)
    }

    /// Validate the external and destination endpoint roles against stored participants.
    pub fn validate_against_participants(
        &self,
        participants: &ParticipantSet,
    ) -> Result<(), ImportIntentValidationError> {
        self.validate_shape()?;
        participants.validate_for_new_operation()?;
        let filesystem = participants
            .descriptors()
            .iter()
            .find(|descriptor| descriptor.id() == self.filesystem_participant_id)
            .ok_or(ImportIntentValidationError::FilesystemParticipantMissing)?;
        if filesystem.kind() != ParticipantKind::Filesystem {
            return Err(
                ImportIntentValidationError::FilesystemParticipantWrongKind {
                    actual: filesystem.kind(),
                },
            );
        }
        if filesystem.requirement() != ParticipantRequirement::Required {
            return Err(ImportIntentValidationError::FilesystemParticipantNotRequired);
        }
        CopyIntentArgs::validate_endpoint(
            participants,
            CopyEndpointRole::Destination,
            &self.destination,
        )
        .map_err(ImportIntentValidationError::Destination)?;
        Ok(())
    }

    /// Return the opaque external-root binding identity.
    pub const fn external_root_binding_id(&self) -> ExternalRootBindingId {
        self.external_root_binding_id
    }

    /// Return the required Filesystem participant identity.
    pub const fn filesystem_participant_id(&self) -> ParticipantId {
        self.filesystem_participant_id
    }

    /// Return the observed external-root physical identity.
    pub fn root_identity(&self) -> &str {
        &self.root_identity
    }

    /// Return the external-root relative input locator.
    pub fn relative_path(&self) -> &StoredRelativePath {
        &self.relative_path
    }

    /// Return the expected external regular-file facts.
    pub fn expected_file(&self) -> &ImportFileExpectation {
        &self.expected_file
    }

    /// Return the destination source-database endpoint.
    pub fn destination(&self) -> &CopyEndpoint {
        &self.destination
    }

    /// Return the explicit no-replace policy.
    pub const fn collision_policy(&self) -> CopyCollisionPolicy {
        self.collision_policy
    }

    /// Return the bounded inherited metadata reference, when supplied.
    pub fn inherited_metadata_reference(&self) -> Option<&str> {
        self.inherited_metadata_reference.as_deref()
    }

    fn validate_shape(&self) -> Result<(), ImportIntentValidationError> {
        validate_bounded_text(&self.root_identity, MAX_IMPORT_ROOT_IDENTITY_BYTES)
            .map_err(|_| ImportIntentValidationError::InvalidRootIdentity)?;
        self.relative_path
            .validate()
            .map_err(ImportIntentValidationError::InvalidPath)?;
        self.expected_file.validate()?;
        self.destination
            .relative_path()
            .validate()
            .map_err(ImportIntentValidationError::InvalidPath)?;
        validate_source_id(self.destination.source_id())
            .map_err(ImportIntentValidationError::Destination)?;
        if let Some(reference) = &self.inherited_metadata_reference {
            validate_bounded_text(reference, MAX_COPY_METADATA_REFERENCE_BYTES)
                .map_err(|_| ImportIntentValidationError::InvalidMetadataReference)?;
        }
        Ok(())
    }
}

/// Errors returned while constructing or validating Import observations.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum ImportIntentValidationError {
    /// The observed external root identity is empty, oversized, or control text.
    #[error("import external root identity is invalid")]
    InvalidRootIdentity,
    /// The external input locator is not portable.
    #[error("invalid import relative path: {0}")]
    InvalidPath(#[from] StoredRelativePathError),
    /// The expected regular-file identity is empty, oversized, or control text.
    #[error("import expected file identity is invalid")]
    InvalidFileIdentity,
    /// The expected regular-file modification timestamp is negative.
    #[error("import expected modification timestamp must be non-negative")]
    NegativeModifiedTimestamp,
    /// The metadata reference is outside its bounded text contract.
    #[error("import inherited metadata reference is invalid")]
    InvalidMetadataReference,
    /// The required Filesystem participant was not found.
    #[error("import Filesystem participant is missing")]
    FilesystemParticipantMissing,
    /// The participant named for external input had another kind.
    #[error("import Filesystem participant has kind {actual:?}")]
    FilesystemParticipantWrongKind {
        /// Actual participant descriptor kind.
        actual: ParticipantKind,
    },
    /// The Filesystem participant was optional rather than required.
    #[error("import Filesystem participant is not required")]
    FilesystemParticipantNotRequired,
    /// The destination endpoint failed source-database role validation.
    #[error("invalid import destination: {0}")]
    Destination(#[from] CopyIntentValidationError),
    /// Participant shape or cross-reference validation failed.
    #[error("invalid import participants: {0}")]
    Participants(#[from] ParticipantValidationError),
}

/// Maximum byte length of a stored waveform file identity observation.
pub const MAX_WAVEFORM_FILE_IDENTITY_BYTES: usize = 256;
/// Maximum byte length of a stored waveform change-marker observation.
pub const MAX_WAVEFORM_CHANGE_MARKER_BYTES: usize = 256;
/// Maximum number of source frames representable by the journal edit envelope.
pub const MAX_WAVEFORM_EDIT_FRAMES: u64 = u32::MAX as u64;

/// Exact finite IEEE-754 single-precision value stored as its bit pattern.
///
/// Persisting the bits rather than a decimal float keeps journal replay equality exact, including
/// signed zero. The value is still an observation and carries no audio or execution authority.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct FiniteF32Bits(u32);

impl FiniteF32Bits {
    /// Construct an exact finite value, retaining its IEEE-754 bit pattern.
    pub fn new(value: f32) -> Result<Self, WaveformEditValidationError> {
        if value.is_finite() {
            Ok(Self(value.to_bits()))
        } else {
            Err(WaveformEditValidationError::NonFiniteFloat)
        }
    }

    /// Wrap a bit pattern after checking that it represents a finite value.
    pub fn from_bits(bits: u32) -> Result<Self, WaveformEditValidationError> {
        Self::new(f32::from_bits(bits))
    }

    /// Return the exact stored IEEE-754 bit pattern.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Return the represented finite value.
    pub fn value(self) -> f32 {
        f32::from_bits(self.0)
    }

    fn in_range(self, minimum: f32, maximum: f32) -> bool {
        let value = self.value();
        value.is_finite() && (minimum..=maximum).contains(&value)
    }
}

/// Stored WAV source sample representation used by an edit observation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum WaveformSampleFormat {
    /// Integer PCM samples.
    Integer,
    /// IEEE-754 single-precision samples.
    Float,
}

/// Bounded source WAV specification captured with an edit request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WaveformSourceSpec {
    channels: u16,
    sample_rate: u32,
    bits_per_sample: u16,
    sample_format: WaveformSampleFormat,
}

impl WaveformSourceSpec {
    /// Construct a source specification supported by the bounded WAV span reader.
    pub fn new(
        channels: u16,
        sample_rate: u32,
        bits_per_sample: u16,
        sample_format: WaveformSampleFormat,
    ) -> Result<Self, WaveformEditValidationError> {
        let spec = Self {
            channels,
            sample_rate,
            bits_per_sample,
            sample_format,
        };
        spec.validate()?;
        Ok(spec)
    }

    /// Validate a source specification recovered from an untrusted record.
    pub fn validate(self) -> Result<(), WaveformEditValidationError> {
        if self.channels == 0 || self.sample_rate == 0 {
            return Err(WaveformEditValidationError::InvalidSourceSpec);
        }
        let supported = match self.sample_format {
            WaveformSampleFormat::Integer => matches!(self.bits_per_sample, 8 | 16 | 24 | 32),
            WaveformSampleFormat::Float => self.bits_per_sample == 32,
        };
        if !supported {
            Err(WaveformEditValidationError::InvalidSourceSpec)
        } else {
            let bytes_per_sample = u64::from(self.bits_per_sample / 8);
            let block_align = u64::from(self.channels).checked_mul(bytes_per_sample);
            let byte_rate =
                block_align.and_then(|align| u64::from(self.sample_rate).checked_mul(align));
            if block_align.is_some_and(|align| align > u64::from(u16::MAX))
                || byte_rate.is_some_and(|rate| rate > u64::from(u32::MAX))
            {
                Err(WaveformEditValidationError::InvalidSourceSpec)
            } else {
                Ok(())
            }
        }
    }

    /// Return the source channel count.
    pub const fn channels(self) -> u16 {
        self.channels
    }

    /// Return the source sample rate.
    pub const fn sample_rate(self) -> u32 {
        self.sample_rate
    }

    /// Return the source bit depth.
    pub const fn bits_per_sample(self) -> u16 {
        self.bits_per_sample
    }

    /// Return the source sample representation.
    pub const fn sample_format(self) -> WaveformSampleFormat {
        self.sample_format
    }
}

/// Output encoding supported by the durable edit observation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum WaveformOutputEncoding {
    /// Native edited-WAV output: IEEE-754 single-precision samples.
    Float32,
}

/// Bounded output specification for an edit request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WaveformOutputSpec {
    encoding: WaveformOutputEncoding,
    channels: u16,
    sample_rate: u32,
}

impl WaveformOutputSpec {
    /// Construct the explicit Float32 output retained by the media edit writer.
    pub const fn float32(channels: u16, sample_rate: u32) -> Self {
        Self {
            encoding: WaveformOutputEncoding::Float32,
            channels,
            sample_rate,
        }
    }

    /// Validate output shape and its equality to the source's channel/rate contract.
    pub fn validate_against_source(
        self,
        source: WaveformSourceSpec,
    ) -> Result<(), WaveformEditValidationError> {
        source.validate()?;
        if self.encoding != WaveformOutputEncoding::Float32
            || self.channels == 0
            || self.sample_rate == 0
            || self.channels != source.channels
            || self.sample_rate != source.sample_rate
        {
            return Err(WaveformEditValidationError::InvalidOutputSpec);
        }
        let block_align = u64::from(self.channels).checked_mul(4);
        let byte_rate =
            block_align.and_then(|align| u64::from(self.sample_rate).checked_mul(align));
        if block_align.is_none_or(|align| align == 0 || align > u64::from(u16::MAX))
            || byte_rate.is_none_or(|rate| rate > u64::from(u32::MAX))
        {
            return Err(WaveformEditValidationError::InvalidOutputSpec);
        }
        Ok(())
    }

    /// Return the output encoding.
    pub const fn encoding(self) -> WaveformOutputEncoding {
        self.encoding
    }

    /// Return the output channel count.
    pub const fn channels(self) -> u16 {
        self.channels
    }

    /// Return the output sample rate.
    pub const fn sample_rate(self) -> u32 {
        self.sample_rate
    }
}

/// Strong file revision observation carried by a durable WAV edit.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WaveformFileRevisionObservation {
    filesystem_identity: String,
    change_marker: String,
    byte_len: u64,
    modified_unix_ns: Option<i64>,
}

impl WaveformFileRevisionObservation {
    /// Construct a bounded file revision observation without opening the file.
    pub fn new(
        filesystem_identity: impl Into<String>,
        change_marker: impl Into<String>,
        byte_len: u64,
        modified_unix_ns: Option<i64>,
    ) -> Result<Self, WaveformEditValidationError> {
        let revision = Self {
            filesystem_identity: filesystem_identity.into(),
            change_marker: change_marker.into(),
            byte_len,
            modified_unix_ns,
        };
        revision.validate()?;
        Ok(revision)
    }

    /// Validate a revision recovered from an untrusted record.
    pub fn validate(&self) -> Result<(), WaveformEditValidationError> {
        validate_bounded_text(&self.filesystem_identity, MAX_WAVEFORM_FILE_IDENTITY_BYTES)
            .map_err(|_| WaveformEditValidationError::InvalidFileRevision)?;
        validate_bounded_text(&self.change_marker, MAX_WAVEFORM_CHANGE_MARKER_BYTES)
            .map_err(|_| WaveformEditValidationError::InvalidFileRevision)?;
        Ok(())
    }

    /// Return the stable filesystem object identity observation.
    pub fn filesystem_identity(&self) -> &str {
        &self.filesystem_identity
    }

    /// Return the source change-marker observation.
    pub fn change_marker(&self) -> &str {
        &self.change_marker
    }

    /// Return the observed byte length.
    pub const fn byte_len(&self) -> u64 {
        self.byte_len
    }

    /// Return the checked signed Unix nanosecond timestamp, when available.
    pub const fn modified_unix_ns(&self) -> Option<i64> {
        self.modified_unix_ns
    }
}

/// Exact half-open source frame range retained by a WAV edit observation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WaveformEditFrameRange {
    start_frame: u64,
    end_frame: u64,
}

impl WaveformEditFrameRange {
    /// Construct a non-empty bounded frame range.
    pub const fn new(
        start_frame: u64,
        end_frame: u64,
    ) -> Result<Self, WaveformEditValidationError> {
        if start_frame < end_frame {
            Ok(Self {
                start_frame,
                end_frame,
            })
        } else {
            Err(WaveformEditValidationError::InvalidFrameRange)
        }
    }

    /// Return the first included source frame.
    pub const fn start_frame(self) -> u64 {
        self.start_frame
    }

    /// Return the exclusive source frame bound.
    pub const fn end_frame(self) -> u64 {
        self.end_frame
    }

    /// Return the checked number of frames in this range.
    pub fn len(self) -> Result<u64, WaveformEditValidationError> {
        self.end_frame
            .checked_sub(self.start_frame)
            .ok_or(WaveformEditValidationError::InvalidFrameRange)
    }
}

/// Exact fade parameters retained by a durable WAV edit.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WaveformEditFadeArgs {
    length: FiniteF32Bits,
    curve: FiniteF32Bits,
    mute: FiniteF32Bits,
    outer_gain: FiniteF32Bits,
}

impl WaveformEditFadeArgs {
    /// Construct exact fade parameters without clamping or changing signed zero.
    pub fn new(
        length: FiniteF32Bits,
        curve: FiniteF32Bits,
        mute: FiniteF32Bits,
        outer_gain: FiniteF32Bits,
    ) -> Result<Self, WaveformEditValidationError> {
        let fade = Self {
            length,
            curve,
            mute,
            outer_gain,
        };
        fade.validate()?;
        Ok(fade)
    }

    /// Validate exact fade parameters recovered from an untrusted record.
    pub fn validate(self) -> Result<(), WaveformEditValidationError> {
        if !self.length.in_range(0.0, 1.0)
            || !self.curve.in_range(0.0, 1.0)
            || self.mute.value() < 0.0
            || !self.mute.value().is_finite()
            || !self.outer_gain.in_range(0.0, 1.0)
        {
            return Err(WaveformEditValidationError::InvalidFade);
        }
        Ok(())
    }

    /// Return the normalized fade length.
    pub const fn length(self) -> FiniteF32Bits {
        self.length
    }

    /// Return the fade curve.
    pub const fn curve(self) -> FiniteF32Bits {
        self.curve
    }

    /// Return the mute amount.
    pub const fn mute(self) -> FiniteF32Bits {
        self.mute
    }

    /// Return the outer gain.
    pub const fn outer_gain(self) -> FiniteF32Bits {
        self.outer_gain
    }
}

/// Exact gain and fade parameters retained by a durable WAV edit.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WaveformEditSettingsArgs {
    gain: FiniteF32Bits,
    fade_in: Option<WaveformEditFadeArgs>,
    fade_out: Option<WaveformEditFadeArgs>,
}

impl WaveformEditSettingsArgs {
    /// Construct exact settings without clamping or changing signed zero.
    pub fn new(
        gain: FiniteF32Bits,
        fade_in: Option<WaveformEditFadeArgs>,
        fade_out: Option<WaveformEditFadeArgs>,
    ) -> Result<Self, WaveformEditValidationError> {
        let settings = Self {
            gain,
            fade_in,
            fade_out,
        };
        settings.validate()?;
        Ok(settings)
    }

    /// Validate settings recovered from an untrusted record.
    pub fn validate(self) -> Result<(), WaveformEditValidationError> {
        if !self.gain.in_range(0.0, 4.0) {
            return Err(WaveformEditValidationError::InvalidGain);
        }
        if let Some(fade) = self.fade_in {
            fade.validate()?;
        }
        if let Some(fade) = self.fade_out {
            fade.validate()?;
        }
        Ok(())
    }

    /// Return the exact gain value.
    pub const fn gain(self) -> FiniteF32Bits {
        self.gain
    }

    /// Return exact fade-in settings, when present.
    pub const fn fade_in(self) -> Option<WaveformEditFadeArgs> {
        self.fade_in
    }

    /// Return exact fade-out settings, when present.
    pub const fn fade_out(self) -> Option<WaveformEditFadeArgs> {
        self.fade_out
    }
}

/// One of the seven bounded waveform edit operations supported by the media editor.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum WaveformEditOperationArgs {
    /// Apply exact gain and optional fades to the selected range.
    GainAndFades(WaveformEditSettingsArgs),
    /// Reverse the selected range.
    Reverse,
    /// Mute the selected range.
    Mute,
    /// Remove the selected range.
    Trim,
    /// Retain only the selected range.
    Crop,
    /// Cyclically slide the whole source by exact signed frames.
    Slide {
        /// Signed cyclic frame offset.
        frame_offset: i64,
    },
    /// Retain/pad signed bounds around the whole source.
    CropWithSilence {
        /// Inclusive signed source-relative start bound.
        start_frame: i64,
        /// Exclusive signed source-relative end bound.
        end_frame: i64,
    },
}

impl WaveformEditOperationArgs {
    fn output_frames(
        self,
        total_frames: u64,
        range: WaveformEditFrameRange,
    ) -> Result<u64, WaveformEditValidationError> {
        let range_len = range.len()?;
        if range.start_frame >= range.end_frame
            || range.end_frame > total_frames
            || total_frames == 0
        {
            return Err(WaveformEditValidationError::InvalidFrameRange);
        }
        match self {
            Self::GainAndFades(settings) => {
                settings.validate()?;
                Ok(total_frames)
            }
            Self::Reverse | Self::Mute => Ok(total_frames),
            Self::Slide { .. } => {
                if matches!(self, Self::Slide { .. })
                    && (range.start_frame != 0 || range.end_frame != total_frames)
                {
                    return Err(WaveformEditValidationError::WholeSourceRequired);
                }
                Ok(total_frames)
            }
            Self::Trim => {
                if range_len == total_frames {
                    return Err(WaveformEditValidationError::EmptyOutput);
                }
                total_frames
                    .checked_sub(range_len)
                    .ok_or(WaveformEditValidationError::InvalidFrameRange)
            }
            Self::Crop => Ok(range_len),
            Self::CropWithSilence {
                start_frame,
                end_frame,
            } => {
                if range.start_frame != 0 || range.end_frame != total_frames {
                    return Err(WaveformEditValidationError::WholeSourceRequired);
                }
                let output = i128::from(end_frame) - i128::from(start_frame);
                if output <= 0 || output > i128::from(MAX_WAVEFORM_EDIT_FRAMES) {
                    return Err(WaveformEditValidationError::InvalidSignedFrameBounds);
                }
                u64::try_from(output)
                    .map_err(|_| WaveformEditValidationError::InvalidSignedFrameBounds)
            }
        }
    }
}

/// Complete storage-neutral typed intent for one in-place WAV edit.
///
/// The source endpoint, revision, operation, and output are observations captured before a
/// later owner performs its own live source and filesystem checks. This value contains no path,
/// open handle, source lease, capacity reservation, or permission to replace a file.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WaveformEditIntentArgs {
    source: CopyEndpoint,
    source_file_revision: WaveformFileRevisionObservation,
    source_spec: WaveformSourceSpec,
    total_frames: u64,
    range: WaveformEditFrameRange,
    operation: WaveformEditOperationArgs,
    output: WaveformOutputSpec,
}

impl WaveformEditIntentArgs {
    /// Construct a bounded edit observation without acquiring any live capability.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        source: CopyEndpoint,
        source_file_revision: WaveformFileRevisionObservation,
        source_spec: WaveformSourceSpec,
        total_frames: u64,
        range: WaveformEditFrameRange,
        operation: WaveformEditOperationArgs,
        output: WaveformOutputSpec,
    ) -> Result<Self, WaveformEditValidationError> {
        let args = Self {
            source,
            source_file_revision,
            source_spec,
            total_frames,
            range,
            operation,
            output,
        };
        args.validate_shape()?;
        Ok(args)
    }

    /// Validate source roles and the required filesystem participant.
    pub fn validate_against_participants(
        &self,
        participants: &ParticipantSet,
    ) -> Result<(), WaveformEditValidationError> {
        self.validate_shape()?;
        participants.validate_for_new_intent()?;
        CopyIntentArgs::validate_endpoint(participants, CopyEndpointRole::Source, &self.source)
            .map_err(WaveformEditValidationError::SourceParticipant)?;
        if !participants.descriptors().iter().any(|descriptor| {
            descriptor.kind() == ParticipantKind::Filesystem
                && descriptor.requirement() == ParticipantRequirement::Required
        }) {
            return Err(WaveformEditValidationError::RequiredFilesystemParticipantMissing);
        }
        Ok(())
    }

    /// Return the source database endpoint observation.
    pub fn source(&self) -> &CopyEndpoint {
        &self.source
    }

    /// Return the strong file revision observation.
    pub fn source_file_revision(&self) -> &WaveformFileRevisionObservation {
        &self.source_file_revision
    }

    /// Return the source WAV specification.
    pub const fn source_spec(&self) -> WaveformSourceSpec {
        self.source_spec
    }

    /// Return the captured source frame count.
    pub const fn total_frames(&self) -> u64 {
        self.total_frames
    }

    /// Return the exact selected source frame range.
    pub const fn range(&self) -> WaveformEditFrameRange {
        self.range
    }

    /// Return the typed edit operation.
    pub const fn operation(&self) -> WaveformEditOperationArgs {
        self.operation
    }

    /// Return the explicit Float32 output specification.
    pub const fn output(&self) -> WaveformOutputSpec {
        self.output
    }

    fn validate_shape(&self) -> Result<(), WaveformEditValidationError> {
        self.source
            .relative_path()
            .validate()
            .map_err(WaveformEditValidationError::InvalidPath)?;
        validate_source_id(self.source.source_id())
            .map_err(WaveformEditValidationError::InvalidSourceIdentity)?;
        self.source_file_revision.validate()?;
        self.source_spec.validate()?;
        if self.total_frames == 0 || self.total_frames > MAX_WAVEFORM_EDIT_FRAMES {
            return Err(WaveformEditValidationError::InvalidFrameCount);
        }
        let output_frames = self
            .operation
            .output_frames(self.total_frames, self.range)?;
        self.output.validate_against_source(self.source_spec)?;
        let samples = output_frames
            .checked_mul(u64::from(self.output.channels()))
            .ok_or(WaveformEditValidationError::OutputArithmeticOverflow)?;
        let output_bytes = samples
            .checked_mul(4)
            .and_then(|bytes| bytes.checked_add(60))
            .ok_or(WaveformEditValidationError::OutputArithmeticOverflow)?;
        if output_bytes > u64::from(u32::MAX) {
            return Err(WaveformEditValidationError::OutputArithmeticOverflow);
        }
        Ok(())
    }
}

/// Failure while constructing or validating a typed waveform edit observation.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum WaveformEditValidationError {
    /// A source endpoint path is not portable.
    #[error("invalid waveform edit source path: {0}")]
    InvalidPath(#[from] StoredRelativePathError),
    /// A source identity is outside its bounded contract.
    #[error("waveform edit source identity is invalid")]
    InvalidSourceIdentity(CopyIntentValidationError),
    /// The strong file revision is malformed or outside its bound.
    #[error("waveform edit file revision is invalid")]
    InvalidFileRevision,
    /// The source WAV specification is unsupported or malformed.
    #[error("waveform edit source specification is invalid")]
    InvalidSourceSpec,
    /// The output is not explicit Float32 matching the source rate/channels.
    #[error("waveform edit output specification is invalid")]
    InvalidOutputSpec,
    /// The captured source frame count is outside its bound.
    #[error("waveform edit frame count is invalid")]
    InvalidFrameCount,
    /// The selected frame range is empty or outside the source.
    #[error("waveform edit frame range is invalid")]
    InvalidFrameRange,
    /// The operation requires a whole-source range.
    #[error("waveform edit operation requires the whole source range")]
    WholeSourceRequired,
    /// The operation would leave no output frames.
    #[error("waveform edit would leave no output frames")]
    EmptyOutput,
    /// Signed padded-crop bounds do not describe a representable output.
    #[error("waveform edit signed frame bounds are invalid")]
    InvalidSignedFrameBounds,
    /// Output sample or byte arithmetic exceeded its checked bounds.
    #[error("waveform edit output arithmetic overflowed its bound")]
    OutputArithmeticOverflow,
    /// Gain settings contain a value outside the exact domain.
    #[error("waveform edit gain is invalid")]
    InvalidGain,
    /// Fade settings contain a value outside the exact domain.
    #[error("waveform edit fade is invalid")]
    InvalidFade,
    /// A stored float was NaN or infinite.
    #[error("waveform edit float is not finite")]
    NonFiniteFloat,
    /// The source participant could not be resolved to a required fenced database.
    #[error("invalid waveform edit source participant: {0}")]
    SourceParticipant(#[source] CopyIntentValidationError),
    /// A required filesystem participant was absent.
    #[error("waveform edit requires a required Filesystem participant")]
    RequiredFilesystemParticipantMissing,
    /// Participant shape validation failed.
    #[error("invalid waveform edit participants: {0}")]
    Participants(#[from] ParticipantValidationError),
}

/// Typed V6 command arguments for the supported regular-file coordinator commands.
///
/// These are bounded observations. They contain no open roots, source leases, capacity
/// reservations, filesystem descriptors, or execution authority.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum UniversalCommandArgs {
    /// Bounded original-owner legacy preparation, never live admission.
    SourceLegacyAdoption(super::legacy_adoption_commands::LegacyAdoptionIntentArgs),
    /// Bounded historical source-only derived layout; not live admission.
    SourceDerivedLayout(super::source_layout_commands::SourceLayoutIntentArgs),
    /// Frozen finite recording; captured bytes are retained separately before dispatch.
    Recording(super::recording_commands::RecordingProposal),
    /// Copy between two source-database endpoints.
    Copy(CopyIntentArgs),
    /// Import one external regular file into a source-database endpoint.
    Import(ImportIntentArgs),
    /// Duplicate one source-database file into another leaf in the same parent directory.
    DuplicateSame(CopyIntentArgs),
    /// Apply one bounded in-place WAV edit to a source-database file.
    WaveformEdit(WaveformEditIntentArgs),
    /// Apply one reviewed click-repair or duplicate-cleanup edit in the full-progress model.
    DecodedEdit(DecodedEditIntentArgs),
    /// Observe one full-progress-only decoded selection export.
    DecodedExport(super::decoded_export_commands::DecodedExportIntentArgs),
    /// Observe one ordered full-progress-only decoded multi-output export.
    DecodedExportBatch(super::decoded_export_batch_commands::DecodedExportBatchIntentArgs),
    /// Rename, move, or create one bounded namespace entry.
    Namespace(NamespaceCommandArgs),
    /// Observe one whole-file render command without executing it.
    Render(super::render_commands::RenderCommandArgs),
    /// Observe one configured-trash or retained-folder removal request.
    Removal(super::removal_commands::RemovalCommandArgs),
    /// Observe one existing retained-folder restore or purge request.
    RetainedResolution(super::retained_resolution_commands::RetainedResolutionCommandArgs),
    /// Observe one selection-only Normalize request with a full-file Float32 output.
    SelectionNormalize(super::selection_normalize_commands::SelectionNormalizeIntentArgs),
    /// Observe one source-owned selection extraction or ordered ExtractAndTrim request.
    SelectionOutput(super::selection_output_commands::SelectionOutputCommandArgs),
    /// Observe one bounded transaction-history Undo or Redo action list.
    History(super::history_commands::HistoryUndoRedoIntentArgs),
    /// Observe one bounded external filesystem reconciliation batch.
    ExternalReconciliation(ExternalReconciliationIntentArgs),
}

impl UniversalCommandArgs {
    /// Return the universal command kind represented by these arguments.
    pub const fn command_kind(&self) -> super::model::UniversalCommandKind {
        match self {
            Self::SourceLegacyAdoption(_) => {
                super::model::UniversalCommandKind::SourceLegacyAdoption
            }
            Self::SourceDerivedLayout(_) => super::model::UniversalCommandKind::SourceDerivedLayout,
            Self::Recording(_) => super::model::UniversalCommandKind::CreateFile,
            Self::Copy(_) => super::model::UniversalCommandKind::Copy,
            Self::Import(_) => super::model::UniversalCommandKind::Import,
            Self::DuplicateSame(_) => super::model::UniversalCommandKind::Duplicate,
            Self::WaveformEdit(_) => super::model::UniversalCommandKind::DestructiveEdit,
            Self::DecodedEdit(_) => super::model::UniversalCommandKind::DestructiveEdit,
            Self::DecodedExport(_) => super::model::UniversalCommandKind::Export,
            Self::DecodedExportBatch(_) => super::model::UniversalCommandKind::Export,
            Self::Namespace(args) => args.command_kind(),
            Self::Render(args) => args.command_kind(),
            Self::Removal(args) => args.command_kind(),
            Self::RetainedResolution(args) => args.command_kind(),
            Self::SelectionNormalize(args) => args.command_kind(),
            Self::SelectionOutput(args) => args.command_kind(),
            Self::History(args) => args.command_kind(),
            Self::ExternalReconciliation(args) => args.command_kind(),
        }
    }

    /// Validate command-specific roles, identities, and bounded locators.
    pub fn validate_against_participants(
        &self,
        participants: &ParticipantSet,
    ) -> Result<(), UniversalCommandArgsValidationError> {
        match self {
            Self::SourceLegacyAdoption(args) => args
                .validate_against_participants(participants)
                .map_err(UniversalCommandArgsValidationError::SourceLegacyAdoption),
            Self::SourceDerivedLayout(args) => args
                .validate_against_participants(participants)
                .map_err(UniversalCommandArgsValidationError::SourceDerivedLayout),
            Self::Recording(args) => args
                .validate_against_participants(participants)
                .map_err(UniversalCommandArgsValidationError::Recording),
            Self::Copy(args) => args
                .validate_against_participants(participants)
                .map_err(UniversalCommandArgsValidationError::Copy),
            Self::Import(args) => args
                .validate_against_participants(participants)
                .map_err(UniversalCommandArgsValidationError::Import),
            Self::DuplicateSame(args) => args
                .validate_as_duplicate_same_against_participants(participants)
                .map_err(UniversalCommandArgsValidationError::DuplicateSame),
            Self::WaveformEdit(args) => args
                .validate_against_participants(participants)
                .map_err(UniversalCommandArgsValidationError::WaveformEdit),
            Self::DecodedEdit(args) => args
                .validate_against_participants(participants)
                .map_err(UniversalCommandArgsValidationError::DecodedEdit),
            Self::DecodedExport(args) => args
                .validate_against_participants(participants)
                .map_err(UniversalCommandArgsValidationError::DecodedExport),
            Self::DecodedExportBatch(args) => args
                .validate_against_participants(participants)
                .map_err(UniversalCommandArgsValidationError::DecodedExportBatch),
            Self::Namespace(args) => args
                .validate_against_participants(participants)
                .map_err(UniversalCommandArgsValidationError::Namespace),
            Self::Render(args) => args
                .validate_against_participants(participants)
                .map_err(UniversalCommandArgsValidationError::Render),
            Self::Removal(args) => args
                .validate_against_participants(participants)
                .map_err(UniversalCommandArgsValidationError::Removal),
            Self::RetainedResolution(args) => args
                .validate_against_participants(participants)
                .map_err(UniversalCommandArgsValidationError::RetainedResolution),
            Self::SelectionNormalize(args) => args
                .validate_against_participants(participants)
                .map_err(UniversalCommandArgsValidationError::SelectionNormalize),
            Self::SelectionOutput(args) => args
                .validate_against_participants(participants)
                .map_err(UniversalCommandArgsValidationError::SelectionOutput),
            Self::History(args) => args
                .validate_against_participants(participants)
                .map_err(UniversalCommandArgsValidationError::History),
            Self::ExternalReconciliation(args) => args
                .validate_against_participants(participants)
                .map_err(UniversalCommandArgsValidationError::ExternalReconciliation),
        }
    }
}

/// Failure while validating a tagged V6 command argument envelope.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum UniversalCommandArgsValidationError {
    /// Original pre-lease legacy identity, policy or topology does not agree.
    #[error("invalid source legacy adoption arguments: {0}")]
    SourceLegacyAdoption(#[source] super::legacy_adoption_commands::LegacyAdoptionInputError),
    /// Source-only layout identity, topology or canonical data was inconsistent.
    #[error("invalid source-derived layout arguments: {0}")]
    SourceDerivedLayout(#[source] super::source_layout_commands::SourceLayoutIntentError),
    /// Finite recording observations failed validation.
    #[error("invalid recording command arguments: {0}")]
    Recording(#[source] super::recording_commands::RecordingValueError),
    /// A source-to-source Copy argument failed validation.
    #[error("invalid Copy command arguments: {0}")]
    Copy(#[source] CopyIntentValidationError),
    /// An external Import argument failed validation.
    #[error("invalid Import command arguments: {0}")]
    Import(#[source] ImportIntentValidationError),
    /// A same-source DuplicateSame argument failed validation.
    #[error("invalid DuplicateSame command arguments: {0}")]
    DuplicateSame(#[source] CopyIntentValidationError),
    /// A bounded in-place waveform edit argument failed validation.
    #[error("invalid WaveformEdit command arguments: {0}")]
    WaveformEdit(#[source] WaveformEditValidationError),
    /// A full-progress-only decoded click-repair or duplicate-cleanup argument failed validation.
    #[error("invalid DecodedEdit command arguments: {0}")]
    DecodedEdit(#[source] super::decoded_edit_commands::DecodedEditValidationError),
    /// A full-progress-only decoded selection-export argument failed validation.
    #[error("invalid DecodedExport command arguments: {0}")]
    DecodedExport(#[source] super::decoded_export_commands::DecodedExportValidationError),
    /// A full-progress-only decoded multi-output export argument failed validation.
    #[error("invalid DecodedExportBatch command arguments: {0}")]
    DecodedExportBatch(
        #[source] super::decoded_export_batch_commands::DecodedExportBatchValidationError,
    ),
    /// A bounded namespace mutation argument failed validation.
    #[error("invalid namespace command arguments: {0}")]
    Namespace(#[source] NamespaceCommandValidationError),
    /// A bounded whole-file render command argument failed validation.
    #[error("invalid render command arguments: {0}")]
    Render(#[source] super::render_commands::RenderCommandValidationError),
    /// A bounded configured-trash or retained-folder removal argument failed validation.
    #[error("invalid removal command arguments: {0}")]
    Removal(#[source] super::removal_commands::RemovalCommandValidationError),
    /// A retained-folder restore or purge argument failed validation.
    #[error("invalid retained resolution command arguments: {0}")]
    RetainedResolution(
        #[source] super::retained_resolution_commands::RetainedResolutionValidationError,
    ),
    /// A selection-only Normalize argument failed validation.
    #[error("invalid selection Normalize command arguments: {0}")]
    SelectionNormalize(
        #[source] super::selection_normalize_commands::SelectionNormalizeValidationError,
    ),
    /// A source-owned selection-output argument failed validation.
    #[error("invalid selection output command arguments: {0}")]
    SelectionOutput(#[source] super::selection_output_commands::SelectionOutputValidationError),
    /// A transaction-history Undo or Redo argument failed validation.
    #[error("invalid history Undo/Redo command arguments: {0}")]
    History(#[source] super::history_commands::HistoryValidationError),
    /// An external filesystem reconciliation observation failed validation.
    #[error("invalid external reconciliation command arguments: {0}")]
    ExternalReconciliation(#[source] ExternalReconciliationValidationError),
}

/// Stable class for an error that may schedule a later retry.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum RetryErrorClass {
    /// The owner encountered a bounded writer or resource contention condition.
    TransientContention,
    /// A participant was temporarily unavailable.
    TransientAvailability,
    /// Capacity was insufficient for the next bounded attempt.
    CapacityUnavailable,
}

/// Opaque durable retry lease identity.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct RetryLeaseId(String);

impl RetryLeaseId {
    /// Construct a non-empty bounded retry lease identifier.
    pub fn new(value: impl Into<String>) -> Result<Self, RetryObservationValidationError> {
        let lease_id = Self(value.into());
        lease_id.validate()?;
        Ok(lease_id)
    }

    /// Borrow the opaque lease identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn validate(&self) -> Result<(), RetryObservationValidationError> {
        validate_bounded_text(&self.0, MAX_RETRY_LEASE_ID_BYTES)
            .map_err(|_| RetryObservationValidationError::InvalidLeaseId)
    }
}

/// Bounded, authority-free scheduling and status observation for a retry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetryObservation {
    attempt: u32,
    requested_unix_ms: i64,
    next_retry_unix_ms: i64,
    lease_id: RetryLeaseId,
    lease_expiry_unix_ms: i64,
    error_class: RetryErrorClass,
    diagnostic_code: String,
    redacted_context: Option<String>,
}

impl RetryObservation {
    /// Construct a bounded retry/status observation without creating a live lease.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        attempt: u32,
        requested_unix_ms: i64,
        next_retry_unix_ms: i64,
        lease_id: RetryLeaseId,
        lease_expiry_unix_ms: i64,
        error_class: RetryErrorClass,
        diagnostic_code: impl Into<String>,
        redacted_context: Option<String>,
    ) -> Result<Self, RetryObservationValidationError> {
        let observation = Self {
            attempt,
            requested_unix_ms,
            next_retry_unix_ms,
            lease_id,
            lease_expiry_unix_ms,
            error_class,
            diagnostic_code: diagnostic_code.into(),
            redacted_context,
        };
        observation.validate()?;
        Ok(observation)
    }

    /// Validate a retry observation recovered from storage.
    pub fn validate(&self) -> Result<(), RetryObservationValidationError> {
        if self.attempt == 0 {
            return Err(RetryObservationValidationError::ZeroAttempt);
        }
        if self.requested_unix_ms < 0
            || self.next_retry_unix_ms < 0
            || self.lease_expiry_unix_ms < 0
        {
            return Err(RetryObservationValidationError::NegativeTimestamp);
        }
        if self.next_retry_unix_ms < self.requested_unix_ms {
            return Err(RetryObservationValidationError::RetryBeforeRequest);
        }
        if self.lease_expiry_unix_ms < self.next_retry_unix_ms {
            return Err(RetryObservationValidationError::LeaseBeforeRetry);
        }
        self.lease_id.validate()?;
        validate_bounded_text(
            &self.diagnostic_code,
            super::participants::MAX_DIAGNOSTIC_CODE_BYTES,
        )
        .map_err(|_| RetryObservationValidationError::InvalidDiagnosticCode)?;
        if let Some(context) = &self.redacted_context {
            validate_bounded_text(context, MAX_RETRY_CONTEXT_BYTES)
                .map_err(|_| RetryObservationValidationError::InvalidContext)?;
        }
        Ok(())
    }

    /// Return the one-based retry attempt number.
    pub const fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Return the request observation timestamp.
    pub const fn requested_unix_ms(&self) -> i64 {
        self.requested_unix_ms
    }

    /// Return the next eligible retry timestamp.
    pub const fn next_retry_unix_ms(&self) -> i64 {
        self.next_retry_unix_ms
    }

    /// Return the opaque durable lease identity.
    pub fn lease_id(&self) -> &RetryLeaseId {
        &self.lease_id
    }

    /// Return the lease expiry observation.
    pub const fn lease_expiry_unix_ms(&self) -> i64 {
        self.lease_expiry_unix_ms
    }

    /// Return the typed retry error class.
    pub const fn error_class(&self) -> RetryErrorClass {
        self.error_class
    }

    /// Return the stable diagnostic code.
    pub fn diagnostic_code(&self) -> &str {
        &self.diagnostic_code
    }

    /// Return bounded redaction-safe context, when present.
    pub fn redacted_context(&self) -> Option<&str> {
        self.redacted_context.as_deref()
    }
}

/// Failure while constructing or validating a retry observation.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum RetryObservationValidationError {
    /// Attempt numbers begin at one.
    #[error("retry attempt must be positive")]
    ZeroAttempt,
    /// Retry timestamps cannot precede the Unix epoch.
    #[error("retry timestamps must be non-negative")]
    NegativeTimestamp,
    /// The next retry cannot precede the request observation.
    #[error("next retry timestamp precedes the request timestamp")]
    RetryBeforeRequest,
    /// The lease must remain valid through the next retry boundary.
    #[error("retry lease expiry precedes the next retry timestamp")]
    LeaseBeforeRetry,
    /// The durable lease identifier is outside its bounded text contract.
    #[error("retry lease ID is invalid")]
    InvalidLeaseId,
    /// The stable diagnostic code is outside its bounded text contract.
    #[error("retry diagnostic code is invalid")]
    InvalidDiagnosticCode,
    /// The redaction-safe context is outside its bounded text contract.
    #[error("retry context is invalid")]
    InvalidContext,
}

pub(crate) fn validate_source_id(source_id: &SourceId) -> Result<(), CopyIntentValidationError> {
    validate_bounded_text(
        source_id.as_str(),
        super::participants::MAX_SOURCE_IDENTITY_BYTES,
    )
    .map_err(|_| CopyIntentValidationError::InvalidSourceIdentity)
}

fn validate_file_identity(value: &str) -> Result<(), CopyIntentValidationError> {
    validate_bounded_text(value, MAX_COPY_CONTENT_IDENTITY_BYTES)
        .map_err(|_| CopyIntentValidationError::InvalidFileIdentity)
}

fn validate_bounded_text(value: &str, maximum: usize) -> Result<(), ()> {
    if value.is_empty() || value.len() > maximum || value.chars().any(char::is_control) {
        Err(())
    } else {
        Ok(())
    }
}

/// Validate the explicit V5 copy-arguments JSON boundary before typed deserialization.
pub(crate) fn validate_persisted_copy_intent_value(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(
        value,
        &[
            "source",
            "destination",
            "expected_source",
            "collision_policy",
            "inherited_metadata_reference",
        ],
    )?;
    validate_persisted_copy_endpoint_value(required_field(object, "source")?)?;
    validate_persisted_copy_endpoint_value(required_field(object, "destination")?)?;
    let expected = strict_object(
        required_field(object, "expected_source")?,
        &[
            "source_revision",
            "file_identity",
            "file_size",
            "modified_ns",
        ],
    )?;
    for field in [
        "source_revision",
        "file_identity",
        "file_size",
        "modified_ns",
    ] {
        required_field(expected, field)?;
    }
    let file_identity = required_field(expected, "file_identity")?
        .as_str()
        .ok_or("copy expected file identity must be a string")?;
    if validate_bounded_text(file_identity, MAX_COPY_CONTENT_IDENTITY_BYTES).is_err() {
        return Err("copy expected file identity is outside its bound");
    }
    let modified_ns = required_field(expected, "modified_ns")?
        .as_i64()
        .ok_or("copy expected modification timestamp must be a signed 64-bit integer")?;
    if modified_ns < 0 {
        return Err("copy expected modification timestamp must be non-negative");
    }
    if !required_field(object, "collision_policy")?.is_string() {
        return Err("copy collision policy must be a string");
    }
    if required_field(object, "collision_policy")?.as_str() != Some("NoReplace") {
        return Err("copy collision policy is unsupported");
    }
    if let Some(reference) = object.get("inherited_metadata_reference") {
        if !reference.is_null() {
            let reference = reference
                .as_str()
                .ok_or("copy inherited metadata reference must be a string or null")?;
            if validate_bounded_text(reference, MAX_COPY_METADATA_REFERENCE_BYTES).is_err() {
                return Err("copy inherited metadata reference is outside its bound");
            }
        }
    }
    Ok(())
}

/// Validate the explicit V6 tagged command-arguments JSON boundary before typed deserialization.
pub(crate) fn validate_persisted_universal_command_args_value(
    value: &Value,
) -> Result<(), &'static str> {
    let object = value
        .as_object()
        .ok_or("persisted command arguments must be a tagged object")?;
    if object.len() != 1 {
        return Err("persisted command arguments must contain exactly one variant");
    }
    let (variant, arguments) = object
        .iter()
        .next()
        .ok_or("persisted command arguments must contain exactly one variant")?;
    match variant.as_str() {
        "Copy" | "DuplicateSame" => validate_persisted_copy_intent_value(arguments),
        "Import" => validate_persisted_import_intent_value(arguments),
        "WaveformEdit" => validate_persisted_waveform_edit_value(arguments),
        "DecodedEdit" => {
            super::decoded_edit_commands::validate_persisted_decoded_edit_value(arguments)
        }
        _ => Err("persisted command arguments contain an unknown variant"),
    }
}

/// Validate the strict V7 waveform-edit command-arguments JSON boundary before typed
/// deserialization. Keeping this boundary borrowed avoids cloning untrusted payloads merely to
/// validate them; the outer record-size and depth checks remain the codec's responsibility.
pub(crate) fn validate_persisted_waveform_edit_value(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(
        value,
        &[
            "source",
            "source_file_revision",
            "source_spec",
            "total_frames",
            "range",
            "operation",
            "output",
        ],
    )?;
    for field in [
        "source",
        "source_file_revision",
        "source_spec",
        "total_frames",
        "range",
        "operation",
        "output",
    ] {
        required_field(object, field)?;
    }
    validate_persisted_copy_endpoint_value(required_field(object, "source")?)?;

    let revision = strict_object(
        required_field(object, "source_file_revision")?,
        &[
            "filesystem_identity",
            "change_marker",
            "byte_len",
            "modified_unix_ns",
        ],
    )?;
    for field in [
        "filesystem_identity",
        "change_marker",
        "byte_len",
        "modified_unix_ns",
    ] {
        required_field(revision, field)?;
    }
    for (field, maximum) in [
        ("filesystem_identity", MAX_WAVEFORM_FILE_IDENTITY_BYTES),
        ("change_marker", MAX_WAVEFORM_CHANGE_MARKER_BYTES),
    ] {
        let text = required_field(revision, field)?
            .as_str()
            .ok_or("waveform edit file revision identity must be a string")?;
        if validate_bounded_text(text, maximum).is_err() {
            return Err("waveform edit file revision identity is outside its bound");
        }
    }
    required_field(revision, "byte_len")?
        .as_u64()
        .ok_or("waveform edit file byte length must be an unsigned integer")?;
    let modified = required_field(revision, "modified_unix_ns")?;
    if !modified.is_null() && modified.as_i64().is_none() {
        return Err("waveform edit modified timestamp must be a signed integer or null");
    }

    let source_spec = strict_object(
        required_field(object, "source_spec")?,
        &[
            "channels",
            "sample_rate",
            "bits_per_sample",
            "sample_format",
        ],
    )?;
    for field in [
        "channels",
        "sample_rate",
        "bits_per_sample",
        "sample_format",
    ] {
        required_field(source_spec, field)?;
    }
    let channels = required_field(source_spec, "channels")?
        .as_u64()
        .ok_or("waveform edit source channels must be an unsigned integer")?;
    let sample_rate = required_field(source_spec, "sample_rate")?
        .as_u64()
        .ok_or("waveform edit source rate must be an unsigned integer")?;
    let bits = required_field(source_spec, "bits_per_sample")?
        .as_u64()
        .ok_or("waveform edit source bit depth must be an unsigned integer")?;
    let format = required_field(source_spec, "sample_format")?
        .as_str()
        .ok_or("waveform edit source format must be a string")?;
    if channels == 0
        || channels > u64::from(u16::MAX)
        || sample_rate == 0
        || sample_rate > u64::from(u32::MAX)
        || bits > u64::from(u16::MAX)
        || !matches!(format, "Integer" | "Float")
    {
        return Err("waveform edit source specification is invalid");
    }
    if (format == "Integer" && !matches!(bits, 8 | 16 | 24 | 32))
        || (format == "Float" && bits != 32)
    {
        return Err("waveform edit source specification is unsupported");
    }

    let total_frames = required_field(object, "total_frames")?
        .as_u64()
        .ok_or("waveform edit total frames must be an unsigned integer")?;
    if total_frames == 0 || total_frames > MAX_WAVEFORM_EDIT_FRAMES {
        return Err("waveform edit total frames are outside their bound");
    }
    let range = strict_object(
        required_field(object, "range")?,
        &["start_frame", "end_frame"],
    )?;
    for field in ["start_frame", "end_frame"] {
        required_field(range, field)?
            .as_u64()
            .ok_or("waveform edit frame range must use unsigned integers")?;
    }
    let start = required_field(range, "start_frame")?.as_u64().unwrap_or(0);
    let end = required_field(range, "end_frame")?.as_u64().unwrap_or(0);
    if start >= end || end > total_frames {
        return Err("waveform edit frame range is invalid");
    }
    validate_persisted_waveform_operation_value(required_field(object, "operation")?)?;

    let output = strict_object(
        required_field(object, "output")?,
        &["encoding", "channels", "sample_rate"],
    )?;
    for field in ["encoding", "channels", "sample_rate"] {
        required_field(output, field)?;
    }
    if required_field(output, "encoding")?.as_str() != Some("Float32")
        || required_field(output, "channels")?.as_u64() != Some(channels)
        || required_field(output, "sample_rate")?.as_u64() != Some(sample_rate)
    {
        return Err("waveform edit output specification does not match source");
    }
    Ok(())
}

fn validate_persisted_waveform_operation_value(value: &Value) -> Result<(), &'static str> {
    if let Some(variant) = value.as_str() {
        return if matches!(variant, "Reverse" | "Mute" | "Trim" | "Crop") {
            Ok(())
        } else {
            Err("waveform edit operation unit variant is unsupported")
        };
    }
    let object = value
        .as_object()
        .ok_or("waveform edit operation must be a tagged value")?;
    if object.len() != 1 {
        return Err("waveform edit operation must contain exactly one variant");
    }
    let (variant, arguments) = object
        .iter()
        .next()
        .ok_or("waveform edit operation must contain exactly one variant")?;
    match variant.as_str() {
        "GainAndFades" => validate_persisted_waveform_settings_value(arguments),
        "Slide" => {
            let object = strict_object(arguments, &["frame_offset"])?;
            required_field(object, "frame_offset")?
                .as_i64()
                .ok_or("waveform edit slide offset must be a signed integer")?;
            Ok(())
        }
        "CropWithSilence" => {
            let object = strict_object(arguments, &["start_frame", "end_frame"])?;
            let start = required_field(object, "start_frame")?
                .as_i64()
                .ok_or("waveform edit padded crop start must be a signed integer")?;
            let end = required_field(object, "end_frame")?
                .as_i64()
                .ok_or("waveform edit padded crop end must be a signed integer")?;
            if end <= start {
                return Err("waveform edit padded crop bounds are invalid");
            }
            Ok(())
        }
        _ => Err("waveform edit operation contains an unknown variant"),
    }
}

fn validate_persisted_waveform_settings_value(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(value, &["gain", "fade_in", "fade_out"])?;
    for field in ["gain", "fade_in", "fade_out"] {
        required_field(object, field)?;
    }
    validate_persisted_finite_f32_bits(required_field(object, "gain")?)?;
    for field in ["fade_in", "fade_out"] {
        let fade = required_field(object, field)?;
        if !fade.is_null() {
            let fade = strict_object(fade, &["length", "curve", "mute", "outer_gain"])?;
            for field in ["length", "curve", "mute", "outer_gain"] {
                validate_persisted_finite_f32_bits(required_field(fade, field)?)?;
            }
        }
    }
    Ok(())
}

fn validate_persisted_finite_f32_bits(value: &Value) -> Result<(), &'static str> {
    let bits = value
        .as_u64()
        .ok_or("waveform edit float must be an unsigned bit pattern")?;
    if bits > u64::from(u32::MAX) || !f32::from_bits(bits as u32).is_finite() {
        return Err("waveform edit float is not a finite f32 bit pattern");
    }
    Ok(())
}

pub(crate) fn validate_persisted_import_intent_value(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(
        value,
        &[
            "external_root_binding_id",
            "filesystem_participant_id",
            "root_identity",
            "relative_path",
            "expected_file",
            "destination",
            "collision_policy",
            "inherited_metadata_reference",
        ],
    )?;
    for field in [
        "external_root_binding_id",
        "filesystem_participant_id",
        "root_identity",
        "relative_path",
        "expected_file",
        "destination",
        "collision_policy",
        "inherited_metadata_reference",
    ] {
        required_field(object, field)?;
    }
    for field in ["external_root_binding_id", "filesystem_participant_id"] {
        if required_field(object, field)?
            .as_str()
            .and_then(|value| uuid::Uuid::parse_str(value).ok())
            .is_none()
        {
            return Err("import participant and root binding IDs must be UUID strings");
        }
    }
    let root_identity = required_field(object, "root_identity")?
        .as_str()
        .ok_or("import root identity must be a string")?;
    if validate_bounded_text(root_identity, MAX_IMPORT_ROOT_IDENTITY_BYTES).is_err() {
        return Err("import root identity is outside its bound");
    }
    let relative_path = required_field(object, "relative_path")?
        .as_str()
        .ok_or("import relative path must be a string")?;
    if StoredRelativePath::new(relative_path).is_err() {
        return Err("import relative path is invalid");
    }
    let expected = strict_object(
        required_field(object, "expected_file")?,
        &["file_identity", "file_size", "modified_ns"],
    )?;
    for field in ["file_identity", "file_size", "modified_ns"] {
        required_field(expected, field)?;
    }
    let file_identity = required_field(expected, "file_identity")?
        .as_str()
        .ok_or("import expected file identity must be a string")?;
    if validate_bounded_text(file_identity, MAX_COPY_CONTENT_IDENTITY_BYTES).is_err() {
        return Err("import expected file identity is outside its bound");
    }
    let modified_ns = required_field(expected, "modified_ns")?
        .as_i64()
        .ok_or("import expected modification timestamp must be a signed 64-bit integer")?;
    if modified_ns < 0 {
        return Err("import expected modification timestamp must be non-negative");
    }
    validate_persisted_copy_endpoint_value(required_field(object, "destination")?)?;
    if required_field(object, "collision_policy")?.as_str() != Some("NoReplace") {
        return Err("import collision policy is unsupported");
    }
    if let Some(reference) = object.get("inherited_metadata_reference") {
        if !reference.is_null() {
            let reference = reference
                .as_str()
                .ok_or("import inherited metadata reference must be a string or null")?;
            if validate_bounded_text(reference, MAX_COPY_METADATA_REFERENCE_BYTES).is_err() {
                return Err("import inherited metadata reference is outside its bound");
            }
        }
    }
    Ok(())
}

fn validate_persisted_copy_endpoint_value(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(value, &["participant_id", "source_id", "relative_path"])?;
    let participant_id = required_field(object, "participant_id")?;
    if participant_id
        .as_str()
        .and_then(|value| uuid::Uuid::parse_str(value).ok())
        .is_none()
    {
        return Err("copy participant ID must be a UUID string");
    }
    let source_id = required_field(object, "source_id")?
        .as_str()
        .ok_or("copy source ID must be a string")?;
    if validate_bounded_text(source_id, super::participants::MAX_SOURCE_IDENTITY_BYTES).is_err() {
        return Err("copy source ID is outside its bound");
    }
    let relative_path = required_field(object, "relative_path")?
        .as_str()
        .ok_or("copy relative path must be a string")?;
    if StoredRelativePath::new(relative_path).is_err() {
        return Err("copy relative path is invalid");
    }
    Ok(())
}

/// Validate the explicit V5 retry-observation JSON boundary before typed deserialization.
pub(crate) fn validate_persisted_retry_observation_value(
    value: &Value,
) -> Result<(), &'static str> {
    let object = strict_object(
        value,
        &[
            "attempt",
            "requested_unix_ms",
            "next_retry_unix_ms",
            "lease_id",
            "lease_expiry_unix_ms",
            "error_class",
            "diagnostic_code",
            "redacted_context",
        ],
    )?;
    let attempt = required_field(object, "attempt")?
        .as_u64()
        .ok_or("retry attempt must be an unsigned integer")?;
    if attempt == 0 || attempt > u32::MAX as u64 {
        return Err("retry attempt is outside its bound");
    }
    for field in [
        "requested_unix_ms",
        "next_retry_unix_ms",
        "lease_expiry_unix_ms",
    ] {
        let timestamp = required_field(object, field)?
            .as_i64()
            .ok_or("retry timestamp must be a signed 64-bit integer")?;
        if timestamp < 0 {
            return Err("retry timestamp must be non-negative");
        }
    }
    let lease_id = required_field(object, "lease_id")?
        .as_str()
        .ok_or("retry lease ID must be a string")?;
    if validate_bounded_text(lease_id, MAX_RETRY_LEASE_ID_BYTES).is_err() {
        return Err("retry lease ID is outside its bound");
    }
    let error_class = required_field(object, "error_class")?
        .as_str()
        .ok_or("retry error class must be a string")?;
    if !matches!(
        error_class,
        "TransientContention" | "TransientAvailability" | "CapacityUnavailable"
    ) {
        return Err("retry error class is unsupported");
    }
    let diagnostic_code = required_field(object, "diagnostic_code")?
        .as_str()
        .ok_or("retry diagnostic code must be a string")?;
    if validate_bounded_text(
        diagnostic_code,
        super::participants::MAX_DIAGNOSTIC_CODE_BYTES,
    )
    .is_err()
    {
        return Err("retry diagnostic code is outside its bound");
    }
    if let Some(context) = object.get("redacted_context") {
        if !context.is_null() {
            let context = context
                .as_str()
                .ok_or("retry context must be a string or null")?;
            if validate_bounded_text(context, MAX_RETRY_CONTEXT_BYTES).is_err() {
                return Err("retry context is outside its bound");
            }
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
        .ok_or("persisted value must be a JSON object")?;
    if object
        .keys()
        .any(|field| !allowed_fields.contains(&field.as_str()))
    {
        return Err("persisted object contains an unknown field");
    }
    Ok(object)
}

fn required_field<'a>(
    object: &'a serde_json::Map<String, Value>,
    field: &str,
) -> Result<&'a Value, &'static str> {
    object
        .get(field)
        .ok_or("persisted object is missing a required field")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operation_journal::{
        ParticipantCheckpoint, ParticipantDescriptor, ParticipantId, ParticipantKind,
        ParticipantRequirement, StoredSourceFence,
    };
    use uuid::Uuid;

    fn participant_set() -> ParticipantSet {
        let source = SourceId::from_string("source-a");
        let fence = StoredSourceFence::new(source.clone(), "root-a", "database-a", 1, 7, 1)
            .expect("source fence");
        let source_id = ParticipantId::new();
        let filesystem_id = ParticipantId::new();
        ParticipantSet::new(
            vec![
                ParticipantDescriptor::new(
                    source_id,
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
                    source_id,
                    super::super::participants::ParticipantCheckpointState::Pending,
                    None,
                    None,
                )
                .expect("source checkpoint"),
                ParticipantCheckpoint::new(
                    filesystem_id,
                    super::super::participants::ParticipantCheckpointState::Pending,
                    None,
                    None,
                )
                .expect("filesystem checkpoint"),
            ],
        )
        .expect("participant set")
    }

    #[test]
    fn stored_relative_path_rejects_nonportable_components() {
        for path in ["", "/absolute", "a//b", "a/./b", "a/../b", "a\\b", "C:/a"] {
            assert!(StoredRelativePath::new(path).is_err(), "accepted {path:?}");
        }
        assert_eq!(
            StoredRelativePath::new("folder/sample.wav")
                .expect("portable path")
                .as_str(),
            "folder/sample.wav"
        );
    }

    #[test]
    fn copy_args_bind_endpoint_to_fenced_source_participant() {
        let participants = participant_set();
        let source_id = participants
            .descriptors()
            .iter()
            .find(|descriptor| descriptor.kind() == ParticipantKind::SourceDatabase)
            .expect("source participant")
            .id();
        let source = CopyEndpoint::new(
            source_id,
            SourceId::from_string("source-a"),
            StoredRelativePath::new("in.wav").expect("source path"),
        )
        .expect("source endpoint");
        let destination = CopyEndpoint::new(
            source_id,
            SourceId::from_string("source-a"),
            StoredRelativePath::new("out.wav").expect("destination path"),
        )
        .expect("destination endpoint");
        let args = CopyIntentArgs::new(
            source,
            destination,
            CopySourceExpectation::new(7, "file-identity", 10, 11).expect("expectation"),
            CopyCollisionPolicy::NoReplace,
            Some(String::from("metadata/entry")),
        )
        .expect("copy args");
        args.validate_against_participants(&participants)
            .expect("source participant validates");

        let filesystem_id = participants
            .descriptors()
            .iter()
            .find(|descriptor| descriptor.kind() == ParticipantKind::Filesystem)
            .expect("filesystem participant")
            .id();
        let wrong_role = CopyIntentArgs::new(
            args.source().clone(),
            CopyEndpoint::new(
                filesystem_id,
                SourceId::from_string("source-a"),
                StoredRelativePath::new("out.wav").expect("destination path"),
            )
            .expect("filesystem endpoint"),
            args.expected_source().clone(),
            CopyCollisionPolicy::NoReplace,
            args.inherited_metadata_reference().map(String::from),
        )
        .expect("wrong role args shape");
        assert!(matches!(
            wrong_role.validate_against_participants(&participants),
            Err(CopyIntentValidationError::ParticipantWrongKind {
                role: CopyEndpointRole::Destination,
                actual: ParticipantKind::Filesystem,
            })
        ));

        let mismatched_source = CopyIntentArgs::new(
            CopyEndpoint::new(
                source_id,
                SourceId::from_string("source-b"),
                StoredRelativePath::new("in.wav").expect("source path"),
            )
            .expect("mismatched source endpoint"),
            args.destination().clone(),
            args.expected_source().clone(),
            CopyCollisionPolicy::NoReplace,
            None,
        )
        .expect("mismatched source args shape");
        assert!(matches!(
            mismatched_source.validate_against_participants(&participants),
            Err(CopyIntentValidationError::ParticipantSourceMismatch {
                role: CopyEndpointRole::Source,
            })
        ));

        let identical = CopyIntentArgs::new(
            args.source().clone(),
            args.source().clone(),
            args.expected_source().clone(),
            CopyCollisionPolicy::NoReplace,
            None,
        )
        .expect("identical endpoint shape");
        assert_eq!(
            identical.validate_against_participants(&participants),
            Err(CopyIntentValidationError::IdenticalEndpoints)
        );

        let source_descriptor = participants
            .descriptors()
            .iter()
            .find(|descriptor| descriptor.id() == source_id)
            .expect("source descriptor")
            .clone();
        let source_only = ParticipantSet::new(
            vec![source_descriptor],
            vec![
                ParticipantCheckpoint::new(
                    source_id,
                    super::super::participants::ParticipantCheckpointState::Pending,
                    None,
                    None,
                )
                .expect("source-only checkpoint"),
            ],
        )
        .expect("source-only participants");
        assert_eq!(
            args.validate_against_participants(&source_only),
            Err(CopyIntentValidationError::RequiredFilesystemParticipantMissing)
        );
    }

    #[test]
    fn retry_observation_requires_ordered_nonnegative_times() {
        let lease = RetryLeaseId::new("retry-1").expect("lease");
        assert!(
            RetryObservation::new(
                1,
                20,
                19,
                lease.clone(),
                21,
                RetryErrorClass::TransientContention,
                "busy",
                None,
            )
            .is_err()
        );
        let observation = RetryObservation::new(
            1,
            20,
            21,
            lease,
            22,
            RetryErrorClass::TransientContention,
            "busy",
            Some(String::from("source writer")),
        )
        .expect("ordered retry");
        assert_eq!(observation.attempt(), 1);
    }

    #[test]
    fn import_args_require_external_filesystem_and_fenced_destination() {
        let participants = participant_set();
        let source_id = participants
            .descriptors()
            .iter()
            .find(|descriptor| descriptor.kind() == ParticipantKind::SourceDatabase)
            .expect("source participant")
            .id();
        let filesystem_id = participants
            .descriptors()
            .iter()
            .find(|descriptor| descriptor.kind() == ParticipantKind::Filesystem)
            .expect("filesystem participant")
            .id();
        let destination = CopyEndpoint::new(
            source_id,
            SourceId::from_string("source-a"),
            StoredRelativePath::new("imports/sample.wav").expect("destination path"),
        )
        .expect("destination endpoint");
        let args = ImportIntentArgs::new(
            ExternalRootBindingId::from_uuid(Uuid::from_u128(1)),
            filesystem_id,
            "external-root-identity",
            StoredRelativePath::new("sample.wav").expect("external path"),
            ImportFileExpectation::new("external-file-identity", 12, 34).expect("file expectation"),
            destination,
            CopyCollisionPolicy::NoReplace,
            Some(String::from("metadata/entry")),
        )
        .expect("import args");
        args.validate_against_participants(&participants)
            .expect("import roles validate");

        let wrong_filesystem = ImportIntentArgs::new(
            args.external_root_binding_id(),
            source_id,
            String::from(args.root_identity()),
            args.relative_path().clone(),
            args.expected_file().clone(),
            args.destination().clone(),
            args.collision_policy(),
            args.inherited_metadata_reference().map(String::from),
        )
        .expect("wrong filesystem shape");
        assert!(matches!(
            wrong_filesystem.validate_against_participants(&participants),
            Err(
                ImportIntentValidationError::FilesystemParticipantWrongKind {
                    actual: ParticipantKind::SourceDatabase
                }
            )
        ));

        let invalid_root = ImportIntentArgs::new(
            args.external_root_binding_id(),
            filesystem_id,
            "",
            args.relative_path().clone(),
            args.expected_file().clone(),
            args.destination().clone(),
            args.collision_policy(),
            None,
        );
        assert_eq!(
            invalid_root,
            Err(ImportIntentValidationError::InvalidRootIdentity)
        );
    }

    #[test]
    fn duplicate_same_requires_one_source_parent_and_distinct_leaf() {
        let participants = participant_set();
        let source_id = participants
            .descriptors()
            .iter()
            .find(|descriptor| descriptor.kind() == ParticipantKind::SourceDatabase)
            .expect("source participant")
            .id();
        let source_identity = SourceId::from_string("source-a");
        let source = CopyEndpoint::new(
            source_id,
            source_identity.clone(),
            StoredRelativePath::new("folder/original.wav").expect("source path"),
        )
        .expect("source endpoint");
        let destination = CopyEndpoint::new(
            source_id,
            source_identity.clone(),
            StoredRelativePath::new("folder/copy.wav").expect("destination path"),
        )
        .expect("destination endpoint");
        let args = CopyIntentArgs::new(
            source,
            destination,
            CopySourceExpectation::new(7, "file-identity", 12, 34).expect("expectation"),
            CopyCollisionPolicy::NoReplace,
            Some(String::from("metadata/entry")),
        )
        .expect("duplicate args");
        args.validate_as_duplicate_same_against_participants(&participants)
            .expect("same-source duplicate validates");

        let different_parent = CopyIntentArgs::new(
            args.source().clone(),
            CopyEndpoint::new(
                source_id,
                source_identity.clone(),
                StoredRelativePath::new("other/copy.wav").expect("other destination"),
            )
            .expect("other destination endpoint"),
            args.expected_source().clone(),
            CopyCollisionPolicy::NoReplace,
            args.inherited_metadata_reference().map(String::from),
        )
        .expect("different parent shape");
        assert_eq!(
            different_parent.validate_as_duplicate_same_against_participants(&participants),
            Err(CopyIntentValidationError::DuplicateParentDirectoryMismatch)
        );

        let different_source = CopyIntentArgs::new(
            args.source().clone(),
            CopyEndpoint::new(
                source_id,
                SourceId::from_string("source-b"),
                StoredRelativePath::new("folder/copy.wav").expect("different source"),
            )
            .expect("different source endpoint"),
            args.expected_source().clone(),
            CopyCollisionPolicy::NoReplace,
            None,
        )
        .expect("different source shape");
        assert!(matches!(
            different_source.validate_as_duplicate_same_against_participants(&participants),
            Err(CopyIntentValidationError::ParticipantSourceMismatch {
                role: CopyEndpointRole::Destination
            })
        ));
    }
}
