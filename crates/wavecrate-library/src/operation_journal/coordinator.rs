//! Strict, storage-neutral coordinator command observations.
//!
//! The first coordinator command contract is deliberately small: a regular-file copy between
//! source-database participants. These values contain source identities, revisions, and bounded
//! relative locators only. They are observations for a later owner; they do not contain a live
//! source lease, filesystem capability, capacity reservation, or permission to execute a copy.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

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

        let source =
            self.validate_endpoint(participants, CopyEndpointRole::Source, &self.source)?;
        self.validate_endpoint(
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

    fn validate_endpoint<'a>(
        &self,
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

fn validate_source_id(source_id: &SourceId) -> Result<(), CopyIntentValidationError> {
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
}
