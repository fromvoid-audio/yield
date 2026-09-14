//! Storage-neutral observations for external filesystem reconciliation.
//!
//! This module captures one bounded batch of backend observations. It deliberately does not
//! retain watcher proof, a filesystem capability, a live root, or a native watcher handle. The
//! source and filesystem participants are only observations that a later owner must revalidate.

use std::ffi::{OsStr, OsString};

#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(windows)]
use std::os::windows::ffi::{OsStrExt, OsStringExt};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;
use uuid::Uuid;

use super::model::UniversalCommandKind;
use super::participants::{
    MAX_SOURCE_IDENTITY_BYTES, ParticipantDescriptor, ParticipantId, ParticipantKind,
    ParticipantRequirement, ParticipantSet, ParticipantValidationError,
};
use crate::sample_sources::SourceId;
use crate::sample_sources::reconciliation::{
    CaptureBoundary, RawEventKind, RawObservation, RawObservationEnvelope, RawObservationLimits,
    RawObservationMetadata, RawPathHint, RawPathRole,
};

/// Maximum raw event count retained by one external reconciliation intent.
pub const MAX_EXTERNAL_RECONCILIATION_EVENTS: usize = 64;
/// Maximum total raw path observations retained by one external reconciliation intent.
pub const MAX_EXTERNAL_RECONCILIATION_PATHS: usize = 512;
/// Maximum durable native path spelling bytes retained by one external reconciliation intent.
pub const MAX_EXTERNAL_RECONCILIATION_PATH_BYTES: usize = 64 * 1024;
/// Maximum durable metadata bytes retained by one external reconciliation intent.
pub const MAX_EXTERNAL_RECONCILIATION_METADATA_BYTES: usize = 32 * 1024;
/// Maximum opaque identity bytes retained in one external reconciliation observation.
pub const MAX_EXTERNAL_RECONCILIATION_IDENTITY_BYTES: usize = MAX_SOURCE_IDENTITY_BYTES;

/// A capture-time evidence batch identity distinct from [`super::model::OperationId`].
///
/// The observing owner supplies this identity before an operation id is assigned. It is an
/// observation key only; it does not provide a deduplication guarantee or establish watcher
/// continuity.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct EvidenceBatchId(Uuid);

impl EvidenceBatchId {
    /// Wrap a caller-supplied UUID.
    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    /// Parse a caller-supplied UUID spelling.
    pub fn parse(value: &str) -> Result<Self, uuid::Error> {
        Uuid::parse_str(value).map(Self)
    }

    /// Return the observed UUID.
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }

    /// Reject a nil identity recovered from storage.
    pub const fn validate(self) -> Result<(), ExternalReconciliationValidationError> {
        if self.0.is_nil() {
            Err(ExternalReconciliationValidationError::NilEvidenceBatch)
        } else {
            Ok(())
        }
    }
}

/// Fixed audit requirement derived for every recovered raw external batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExternalReconciliationAuditRequirement {
    /// A later source owner must audit the source before acting on this raw batch.
    SourceAuditRequired,
}

/// Lossless platform-tagged native text used for paths and optional backend strings.
///
/// Unix values retain the exact `OsStrExt::as_bytes` bytes, including bytes that are not UTF-8.
/// Windows values retain exact UTF-16 code units, including unpaired surrogates. The variants are
/// never reconstructed through a foreign platform's native encoding.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum ExternalNativeString {
    /// Exact Unix native bytes.
    UnixBytes(Vec<u8>),
    /// Exact Windows UTF-16 code units.
    WindowsUtf16(Vec<u16>),
}

impl ExternalNativeString {
    /// Construct a Unix byte spelling for a typed cross-platform fixture.
    pub fn unix_bytes(bytes: Vec<u8>) -> Result<Self, ExternalReconciliationValidationError> {
        Ok(Self::UnixBytes(bytes))
    }

    /// Construct a Windows UTF-16 spelling for a typed cross-platform fixture.
    pub fn windows_utf16(units: Vec<u16>) -> Result<Self, ExternalReconciliationValidationError> {
        Ok(Self::WindowsUtf16(units))
    }

    /// Capture one native string without lossy Unicode conversion.
    pub fn from_os_str(value: &OsStr) -> Result<Self, ExternalReconciliationValidationError> {
        #[cfg(unix)]
        {
            return Self::unix_bytes(value.as_bytes().to_vec());
        }
        #[cfg(windows)]
        {
            return Self::windows_utf16(value.encode_wide().collect());
        }
        #[allow(unreachable_code)]
        Err(ExternalReconciliationValidationError::UnsupportedHostPlatform)
    }

    /// Rebuild a native string only when the stored platform matches the running platform.
    pub fn try_to_os_string(&self) -> Result<OsString, ExternalReconciliationValidationError> {
        #[cfg(unix)]
        {
            return match self {
                Self::UnixBytes(bytes) => Ok(OsString::from_vec(bytes.clone())),
                Self::WindowsUtf16(_) => {
                    Err(ExternalReconciliationValidationError::ForeignPlatformString)
                }
            };
        }
        #[cfg(windows)]
        {
            return match self {
                Self::UnixBytes(_) => {
                    Err(ExternalReconciliationValidationError::ForeignPlatformString)
                }
                Self::WindowsUtf16(units) => Ok(OsString::from_wide(units)),
            };
        }
        #[allow(unreachable_code)]
        Err(ExternalReconciliationValidationError::UnsupportedHostPlatform)
    }

    /// Borrow the exact Unix bytes when this is a Unix value.
    pub fn unix_bytes_ref(&self) -> Option<&[u8]> {
        match self {
            Self::UnixBytes(bytes) => Some(bytes),
            Self::WindowsUtf16(_) => None,
        }
    }

    /// Borrow the exact Windows UTF-16 units when this is a Windows value.
    pub fn windows_utf16_ref(&self) -> Option<&[u16]> {
        match self {
            Self::UnixBytes(_) => None,
            Self::WindowsUtf16(units) => Some(units),
        }
    }

    /// Return the durable byte count, using two bytes per UTF-16 code unit.
    pub fn durable_byte_len(&self) -> Result<usize, ExternalReconciliationValidationError> {
        match self {
            Self::UnixBytes(bytes) => Ok(bytes.len()),
            Self::WindowsUtf16(units) => units
                .len()
                .checked_mul(2)
                .ok_or(ExternalReconciliationValidationError::ArithmeticOverflow),
        }
    }

    fn validate_length(&self, maximum: usize) -> Result<(), ExternalReconciliationValidationError> {
        if self.durable_byte_len()? > maximum {
            Err(ExternalReconciliationValidationError::NativeStringTooLong)
        } else {
            Ok(())
        }
    }
}

/// Backend-neutral event kind retained in durable external evidence.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ExternalRawEventKind {
    /// A path was created.
    Create,
    /// A path was modified.
    Modify,
    /// A path was deleted.
    Delete,
    /// A source path moved to a destination.
    Rename,
    /// A source path was copied to a destination.
    Copy,
    /// The source root changed.
    RootChanged,
    /// The backend reported overflow or delivery loss.
    Overflow,
    /// The backend reported an error.
    Error,
    /// The backend delivered an unsupported event shape.
    Unsupported,
}

impl From<RawEventKind> for ExternalRawEventKind {
    fn from(value: RawEventKind) -> Self {
        match value {
            RawEventKind::Create => Self::Create,
            RawEventKind::Modify => Self::Modify,
            RawEventKind::Delete => Self::Delete,
            RawEventKind::Rename => Self::Rename,
            RawEventKind::Copy => Self::Copy,
            RawEventKind::RootChanged => Self::RootChanged,
            RawEventKind::Overflow => Self::Overflow,
            RawEventKind::Error => Self::Error,
            RawEventKind::Unsupported => Self::Unsupported,
        }
    }
}

/// Role retained for one raw path observation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ExternalRawPathRole {
    /// Generic event subject.
    Subject,
    /// Rename source.
    RenameSource,
    /// Rename destination.
    RenameDestination,
    /// Copy source.
    CopySource,
    /// Copy destination.
    CopyDestination,
}

impl From<RawPathRole> for ExternalRawPathRole {
    fn from(value: RawPathRole) -> Self {
        match value {
            RawPathRole::Subject => Self::Subject,
            RawPathRole::RenameSource => Self::RenameSource,
            RawPathRole::RenameDestination => Self::RenameDestination,
            RawPathRole::CopySource => Self::CopySource,
            RawPathRole::CopyDestination => Self::CopyDestination,
        }
    }
}

/// Optional entry-type hint retained for one raw path observation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ExternalRawPathHint {
    /// Directory hint.
    Directory,
    /// Empty-folder marker.
    EmptyFolder,
    /// Symlink or reparse-point hint.
    Symlink,
    /// Observed absence hint.
    Absent,
    /// Explicitly unknown entry type.
    Unknown,
    /// Physical source root hint.
    Root,
}

impl From<RawPathHint> for ExternalRawPathHint {
    fn from(value: RawPathHint) -> Self {
        match value {
            RawPathHint::Directory => Self::Directory,
            RawPathHint::EmptyFolder => Self::EmptyFolder,
            RawPathHint::Symlink => Self::Symlink,
            RawPathHint::Absent => Self::Absent,
            RawPathHint::Unknown => Self::Unknown,
            RawPathHint::Root => Self::Root,
        }
    }
}

/// One lossless raw path observation, retaining order, role, hints, and duplicates.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalRawObservedPath {
    path: ExternalNativeString,
    role: ExternalRawPathRole,
    hint: Option<ExternalRawPathHint>,
}

impl ExternalRawObservedPath {
    /// Construct one path observation without normalizing its spelling.
    pub fn new(
        path: ExternalNativeString,
        role: ExternalRawPathRole,
        hint: Option<ExternalRawPathHint>,
    ) -> Result<Self, ExternalReconciliationValidationError> {
        path.validate_length(MAX_EXTERNAL_RECONCILIATION_PATH_BYTES)?;
        Ok(Self { path, role, hint })
    }

    /// Borrow the exact platform-tagged path spelling.
    pub fn path(&self) -> &ExternalNativeString {
        &self.path
    }

    /// Return the retained path role.
    pub const fn role(&self) -> ExternalRawPathRole {
        self.role
    }

    /// Return the optional retained path hint.
    pub const fn hint(&self) -> Option<ExternalRawPathHint> {
        self.hint
    }
}

/// Optional backend metadata retained on one external raw observation.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalRawObservationMetadata {
    flags: u64,
    rename_cookie: Option<u64>,
    event_id: Option<u64>,
    cursor: Option<Vec<u8>>,
    detail: Option<ExternalNativeString>,
    source: Option<ExternalNativeString>,
    process_id: Option<u32>,
}

impl ExternalRawObservationMetadata {
    /// Construct empty optional metadata.
    pub const fn new() -> Self {
        Self {
            flags: 0,
            rename_cookie: None,
            event_id: None,
            cursor: None,
            detail: None,
            source: None,
            process_id: None,
        }
    }

    /// Retain backend flags.
    pub fn with_flags(mut self, value: u64) -> Self {
        self.flags = value;
        self
    }

    /// Retain a rename cookie.
    pub fn with_rename_cookie(mut self, value: u64) -> Self {
        self.rename_cookie = Some(value);
        self
    }

    /// Retain a backend event identifier.
    pub fn with_event_id(mut self, value: u64) -> Self {
        self.event_id = Some(value);
        self
    }

    /// Retain opaque cursor bytes.
    pub fn with_cursor(mut self, value: Vec<u8>) -> Self {
        self.cursor = Some(value);
        self
    }

    /// Retain native backend detail.
    pub fn with_detail(mut self, value: ExternalNativeString) -> Self {
        self.detail = Some(value);
        self
    }

    /// Retain native backend source text.
    pub fn with_source(mut self, value: ExternalNativeString) -> Self {
        self.source = Some(value);
        self
    }

    /// Retain an origin process ID.
    pub fn with_process_id(mut self, value: u32) -> Self {
        self.process_id = Some(value);
        self
    }

    /// Return backend flags.
    pub const fn flags(&self) -> u64 {
        self.flags
    }

    /// Return the optional rename cookie.
    pub const fn rename_cookie(&self) -> Option<u64> {
        self.rename_cookie
    }

    /// Return the optional backend event identifier.
    pub const fn event_id(&self) -> Option<u64> {
        self.event_id
    }

    /// Borrow opaque cursor bytes.
    pub fn cursor(&self) -> Option<&[u8]> {
        self.cursor.as_deref()
    }

    /// Borrow native backend detail.
    pub fn detail(&self) -> Option<&ExternalNativeString> {
        self.detail.as_ref()
    }

    /// Borrow native backend source text.
    pub fn source(&self) -> Option<&ExternalNativeString> {
        self.source.as_ref()
    }

    /// Return the optional origin process ID.
    pub const fn process_id(&self) -> Option<u32> {
        self.process_id
    }

    fn from_raw(
        metadata: &RawObservationMetadata,
    ) -> Result<Self, ExternalReconciliationValidationError> {
        Ok(Self {
            flags: metadata.flags(),
            rename_cookie: metadata.rename_cookie(),
            event_id: metadata.event_id(),
            cursor: metadata.cursor().map(ToOwned::to_owned),
            detail: metadata
                .detail()
                .map(ExternalNativeString::from_os_str)
                .transpose()?,
            source: metadata
                .source()
                .map(ExternalNativeString::from_os_str)
                .transpose()?,
            process_id: metadata.process_id(),
        })
    }
}

/// One ordered raw external observation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalRawObservation {
    kind: ExternalRawEventKind,
    paths: Vec<ExternalRawObservedPath>,
    metadata: ExternalRawObservationMetadata,
    uncertainty_bits: u16,
}

impl ExternalRawObservation {
    /// Construct one ordered observation, retaining duplicate paths.
    pub fn new(
        kind: ExternalRawEventKind,
        paths: Vec<ExternalRawObservedPath>,
        metadata: ExternalRawObservationMetadata,
        uncertainty_bits: u16,
    ) -> Result<Self, ExternalReconciliationValidationError> {
        let observation = Self {
            kind,
            paths,
            metadata,
            uncertainty_bits,
        };
        observation.validate()?;
        Ok(observation)
    }

    /// Return the retained event kind.
    pub const fn kind(&self) -> ExternalRawEventKind {
        self.kind
    }

    /// Borrow paths in their captured order.
    pub fn paths(&self) -> &[ExternalRawObservedPath] {
        &self.paths
    }

    /// Borrow optional event metadata.
    pub const fn metadata(&self) -> &ExternalRawObservationMetadata {
        &self.metadata
    }

    /// Return all uncertainty bits, including unknown bits.
    pub const fn uncertainty_bits(&self) -> u16 {
        self.uncertainty_bits
    }

    /// Validate this observation's local bounds.
    pub fn validate(&self) -> Result<(), ExternalReconciliationValidationError> {
        let mut path_bytes = 0usize;
        for path in &self.paths {
            path.path
                .validate_length(MAX_EXTERNAL_RECONCILIATION_PATH_BYTES)?;
            path_bytes = path_bytes
                .checked_add(path.path.durable_byte_len()?)
                .ok_or(ExternalReconciliationValidationError::ArithmeticOverflow)?;
        }
        if path_bytes > MAX_EXTERNAL_RECONCILIATION_PATH_BYTES {
            return Err(ExternalReconciliationValidationError::PathBytesExceeded);
        }
        Ok(())
    }

    fn from_raw(
        observation: &RawObservation,
    ) -> Result<Self, ExternalReconciliationValidationError> {
        let mut paths = Vec::with_capacity(observation.paths().len());
        for path in observation.paths() {
            paths.push(ExternalRawObservedPath::new(
                ExternalNativeString::from_os_str(path.path().as_os_str())?,
                path.role().into(),
                path.hint().map(Into::into),
            )?);
        }
        Self::new(
            observation.kind().into(),
            paths,
            ExternalRawObservationMetadata::from_raw(observation.metadata())?,
            observation.uncertainty().bits(),
        )
    }
}

/// Capture boundary retained without collapsing missing or one-sided sequence evidence.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalCaptureBoundary {
    captured_at: u64,
    first_sequence: Option<u64>,
    last_sequence: Option<u64>,
}

impl ExternalCaptureBoundary {
    /// Construct a capture boundary.
    pub const fn new(
        captured_at: u64,
        first_sequence: Option<u64>,
        last_sequence: Option<u64>,
    ) -> Result<Self, ExternalReconciliationValidationError> {
        if let (Some(first), Some(last)) = (first_sequence, last_sequence)
            && first > last
        {
            return Err(ExternalReconciliationValidationError::InvalidCaptureBoundary);
        }
        Ok(Self {
            captured_at,
            first_sequence,
            last_sequence,
        })
    }

    /// Return the capture marker.
    pub const fn captured_at(self) -> u64 {
        self.captured_at
    }

    /// Return the optional first sequence.
    pub const fn first_sequence(self) -> Option<u64> {
        self.first_sequence
    }

    /// Return the optional last sequence.
    pub const fn last_sequence(self) -> Option<u64> {
        self.last_sequence
    }

    /// Classify exact, one-sided, or absent sequence evidence.
    pub const fn sequence_evidence(self) -> ExternalCaptureSequenceEvidence {
        match (self.first_sequence, self.last_sequence) {
            (None, None) => ExternalCaptureSequenceEvidence::Missing,
            (Some(first), Some(last)) => ExternalCaptureSequenceEvidence::Exact { first, last },
            _ => ExternalCaptureSequenceEvidence::Ambiguous,
        }
    }
}

impl From<CaptureBoundary> for ExternalCaptureBoundary {
    fn from(value: CaptureBoundary) -> Self {
        Self {
            captured_at: value.captured_at(),
            first_sequence: value.first_sequence(),
            last_sequence: value.last_sequence(),
        }
    }
}

/// Durable classification of capture sequence endpoint evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExternalCaptureSequenceEvidence {
    /// Neither endpoint was supplied.
    Missing,
    /// Exactly one endpoint was supplied.
    Ambiguous,
    /// Both endpoints form an ordered inclusive range.
    Exact {
        /// First sequence endpoint.
        first: u64,
        /// Last sequence endpoint.
        last: u64,
    },
}

/// Opaque raw provenance retained from the observing backend.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalRawObservationProvenance {
    source_id: SourceId,
    root_identity: Option<Vec<u8>>,
    backend_stream_identity: Option<Vec<u8>>,
    watcher_generation: u64,
    capture_boundary: ExternalCaptureBoundary,
}

impl ExternalRawObservationProvenance {
    /// Construct opaque provenance observations.
    pub fn new(
        source_id: SourceId,
        root_identity: Option<Vec<u8>>,
        backend_stream_identity: Option<Vec<u8>>,
        watcher_generation: u64,
        capture_boundary: ExternalCaptureBoundary,
    ) -> Result<Self, ExternalReconciliationValidationError> {
        let provenance = Self {
            source_id,
            root_identity,
            backend_stream_identity,
            watcher_generation,
            capture_boundary,
        };
        provenance.validate()?;
        Ok(provenance)
    }

    /// Borrow the observed source identity.
    pub fn source_id(&self) -> &SourceId {
        &self.source_id
    }

    /// Borrow opaque backend root bytes.
    pub fn root_identity(&self) -> Option<&[u8]> {
        self.root_identity.as_deref()
    }

    /// Borrow opaque backend stream bytes.
    pub fn backend_stream_identity(&self) -> Option<&[u8]> {
        self.backend_stream_identity.as_deref()
    }

    /// Return the backend watcher generation, distinct from source lifecycle generation.
    pub const fn watcher_generation(&self) -> u64 {
        self.watcher_generation
    }

    /// Return the retained capture boundary.
    pub const fn capture_boundary(&self) -> ExternalCaptureBoundary {
        self.capture_boundary
    }

    fn validate(&self) -> Result<(), ExternalReconciliationValidationError> {
        validate_source_id(&self.source_id)?;
        for identity in [&self.root_identity, &self.backend_stream_identity]
            .into_iter()
            .flatten()
        {
            if identity.len() > MAX_EXTERNAL_RECONCILIATION_IDENTITY_BYTES {
                return Err(ExternalReconciliationValidationError::IdentityTooLong);
            }
        }
        if let (Some(first), Some(last)) = (
            self.capture_boundary.first_sequence(),
            self.capture_boundary.last_sequence(),
        ) && first > last
        {
            return Err(ExternalReconciliationValidationError::InvalidCaptureBoundary);
        }
        Ok(())
    }
}

impl From<&RawObservationEnvelope> for ExternalRawObservationProvenance {
    fn from(value: &RawObservationEnvelope) -> Self {
        Self {
            source_id: value.provenance().source_id().clone(),
            root_identity: value
                .provenance()
                .root_identity()
                .map(|identity| identity.as_bytes().to_vec()),
            backend_stream_identity: value
                .provenance()
                .backend_stream_identity()
                .map(|identity| identity.as_bytes().to_vec()),
            watcher_generation: value.provenance().watcher_generation().get(),
            capture_boundary: value.provenance().capture_boundary().into(),
        }
    }
}

/// Original native limits retained as descriptive source observations.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalRawObservationLimits {
    max_events: u64,
    max_native_path_bytes: u64,
    max_native_metadata_bytes: u64,
}

impl ExternalRawObservationLimits {
    /// Construct the original native accounting limits.
    pub const fn new(
        max_events: u64,
        max_native_path_bytes: u64,
        max_native_metadata_bytes: u64,
    ) -> Result<Self, ExternalReconciliationValidationError> {
        if max_events == 0 {
            return Err(ExternalReconciliationValidationError::ZeroNativeEventLimit);
        }
        Ok(Self {
            max_events,
            max_native_path_bytes,
            max_native_metadata_bytes,
        })
    }

    /// Return the original native event limit.
    pub const fn max_events(self) -> u64 {
        self.max_events
    }

    /// Return the original native path-byte limit.
    pub const fn max_native_path_bytes(self) -> u64 {
        self.max_native_path_bytes
    }

    /// Return the original native metadata-byte limit.
    pub const fn max_native_metadata_bytes(self) -> u64 {
        self.max_native_metadata_bytes
    }
}

impl TryFrom<RawObservationLimits> for ExternalRawObservationLimits {
    type Error = ExternalReconciliationValidationError;

    fn try_from(value: RawObservationLimits) -> Result<Self, Self::Error> {
        Self::new(
            u64::try_from(value.max_events())
                .map_err(|_| ExternalReconciliationValidationError::ArithmeticOverflow)?,
            u64::try_from(value.max_path_bytes())
                .map_err(|_| ExternalReconciliationValidationError::ArithmeticOverflow)?,
            u64::try_from(value.max_metadata_bytes())
                .map_err(|_| ExternalReconciliationValidationError::ArithmeticOverflow)?,
        )
    }
}

/// Original native accounting retained independently from durable UTF-16 accounting.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalRawNativeAccounting {
    event_count: u64,
    path_bytes: u64,
    metadata_bytes: u64,
}

impl ExternalRawNativeAccounting {
    /// Construct descriptive source accounting values.
    pub const fn new(event_count: u64, path_bytes: u64, metadata_bytes: u64) -> Self {
        Self {
            event_count,
            path_bytes,
            metadata_bytes,
        }
    }

    /// Return the source event count.
    pub const fn event_count(self) -> u64 {
        self.event_count
    }

    /// Return source-native path bytes.
    pub const fn path_bytes(self) -> u64 {
        self.path_bytes
    }

    /// Return source-native metadata bytes.
    pub const fn metadata_bytes(self) -> u64 {
        self.metadata_bytes
    }
}

/// Durable accounting measured after platform-tagged conversion.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalRawDurableAccounting {
    event_count: u64,
    path_bytes: u64,
    metadata_bytes: u64,
}

impl ExternalRawDurableAccounting {
    /// Return durable event count.
    pub const fn event_count(self) -> u64 {
        self.event_count
    }

    /// Return durable path bytes.
    pub const fn path_bytes(self) -> u64 {
        self.path_bytes
    }

    /// Return durable metadata bytes.
    pub const fn metadata_bytes(self) -> u64 {
        self.metadata_bytes
    }
}

/// Bounded raw observation envelope retained by V15.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalRawObservationEnvelope {
    provenance: ExternalRawObservationProvenance,
    limits: ExternalRawObservationLimits,
    native_accounting: ExternalRawNativeAccounting,
    durable_accounting: ExternalRawDurableAccounting,
    observations: Vec<ExternalRawObservation>,
}

impl ExternalRawObservationEnvelope {
    /// Construct a bounded durable envelope from typed stored observations.
    pub fn new(
        provenance: ExternalRawObservationProvenance,
        limits: ExternalRawObservationLimits,
        native_accounting: ExternalRawNativeAccounting,
        observations: Vec<ExternalRawObservation>,
    ) -> Result<Self, ExternalReconciliationValidationError> {
        let envelope = Self {
            provenance,
            limits,
            native_accounting,
            durable_accounting: ExternalRawDurableAccounting {
                event_count: 0,
                path_bytes: 0,
                metadata_bytes: 0,
            },
            observations,
        };
        let durable_accounting = envelope.compute_durable_accounting()?;
        let envelope = Self {
            durable_accounting,
            ..envelope
        };
        envelope.validate()?;
        Ok(envelope)
    }

    /// Convert a borrowed native raw envelope without reconstructing its proof.
    pub fn from_raw_envelope(
        value: &RawObservationEnvelope,
    ) -> Result<Self, ExternalReconciliationValidationError> {
        let event_count = value.observations().len();
        if event_count == 0 || event_count > MAX_EXTERNAL_RECONCILIATION_EVENTS {
            return Err(ExternalReconciliationValidationError::EventCountExceeded);
        }

        // Count before allocating the durable vectors. This keeps conversion preallocation
        // bounded by checked source observations rather than attacker-controlled capacities.
        let mut path_count = 0usize;
        let mut path_bytes = 0usize;
        let mut metadata_bytes = 0usize;
        for observation in value.observations() {
            path_count = path_count
                .checked_add(observation.paths().len())
                .ok_or(ExternalReconciliationValidationError::ArithmeticOverflow)?;
            if path_count > MAX_EXTERNAL_RECONCILIATION_PATHS {
                return Err(ExternalReconciliationValidationError::PathCountExceeded);
            }
            for path in observation.paths() {
                path_bytes = path_bytes
                    .checked_add(native_os_str_len(path.path().as_os_str())?)
                    .ok_or(ExternalReconciliationValidationError::ArithmeticOverflow)?;
            }
            metadata_bytes = metadata_bytes
                .checked_add(raw_observation_durable_metadata_bytes(observation)?)
                .ok_or(ExternalReconciliationValidationError::ArithmeticOverflow)?;
        }
        metadata_bytes = metadata_bytes
            .checked_add(raw_provenance_metadata_bytes(value)?)
            .ok_or(ExternalReconciliationValidationError::ArithmeticOverflow)?;
        if path_bytes > MAX_EXTERNAL_RECONCILIATION_PATH_BYTES {
            return Err(ExternalReconciliationValidationError::PathBytesExceeded);
        }
        if metadata_bytes > MAX_EXTERNAL_RECONCILIATION_METADATA_BYTES {
            return Err(ExternalReconciliationValidationError::MetadataBytesExceeded);
        }

        let mut observations = Vec::with_capacity(event_count);
        for observation in value.observations() {
            observations.push(ExternalRawObservation::from_raw(observation)?);
        }
        Self::new(
            ExternalRawObservationProvenance::from(value),
            value.limits().try_into()?,
            ExternalRawNativeAccounting::new(
                u64::try_from(value.accounting().event_count())
                    .map_err(|_| ExternalReconciliationValidationError::ArithmeticOverflow)?,
                u64::try_from(value.accounting().path_bytes())
                    .map_err(|_| ExternalReconciliationValidationError::ArithmeticOverflow)?,
                u64::try_from(value.accounting().metadata_bytes())
                    .map_err(|_| ExternalReconciliationValidationError::ArithmeticOverflow)?,
            ),
            observations,
        )
    }

    /// Borrow raw provenance.
    pub fn provenance(&self) -> &ExternalRawObservationProvenance {
        &self.provenance
    }

    /// Return original native limits.
    pub const fn limits(&self) -> ExternalRawObservationLimits {
        self.limits
    }

    /// Return original native accounting.
    pub const fn native_accounting(&self) -> ExternalRawNativeAccounting {
        self.native_accounting
    }

    /// Return durable UTF-8/UTF-16-unit accounting.
    pub const fn durable_accounting(&self) -> ExternalRawDurableAccounting {
        self.durable_accounting
    }

    /// Borrow ordered raw observations.
    pub fn observations(&self) -> &[ExternalRawObservation] {
        &self.observations
    }

    /// Validate all envelope bounds and derived accounting.
    pub fn validate(&self) -> Result<(), ExternalReconciliationValidationError> {
        self.provenance.validate()?;
        if self.observations.is_empty()
            || self.observations.len() > MAX_EXTERNAL_RECONCILIATION_EVENTS
        {
            return Err(ExternalReconciliationValidationError::EventCountExceeded);
        }
        let expected = self.compute_durable_accounting()?;
        if expected != self.durable_accounting {
            return Err(ExternalReconciliationValidationError::DurableAccountingMismatch);
        }
        if self.native_accounting.event_count != self.observations.len() as u64 {
            return Err(ExternalReconciliationValidationError::NativeAccountingMismatch);
        }
        self.limits.validate()?;
        Ok(())
    }

    fn compute_durable_accounting(
        &self,
    ) -> Result<ExternalRawDurableAccounting, ExternalReconciliationValidationError> {
        let mut path_count = 0usize;
        let mut path_bytes = 0usize;
        let mut metadata_bytes = raw_provenance_metadata_bytes_without_source(&self.provenance)?;
        metadata_bytes = metadata_bytes
            .checked_add(
                usize::try_from(self.provenance.source_id.as_str().len())
                    .map_err(|_| ExternalReconciliationValidationError::ArithmeticOverflow)?,
            )
            .ok_or(ExternalReconciliationValidationError::ArithmeticOverflow)?;
        for observation in &self.observations {
            observation.validate()?;
            path_count = path_count
                .checked_add(observation.paths.len())
                .ok_or(ExternalReconciliationValidationError::ArithmeticOverflow)?;
            for path in &observation.paths {
                path_bytes = path_bytes
                    .checked_add(path.path.durable_byte_len()?)
                    .ok_or(ExternalReconciliationValidationError::ArithmeticOverflow)?;
                metadata_bytes = metadata_bytes
                    .checked_add(1 + usize::from(path.hint.is_some()))
                    .ok_or(ExternalReconciliationValidationError::ArithmeticOverflow)?;
            }
            metadata_bytes = metadata_bytes
                .checked_add(observation_durable_metadata_bytes(observation)?)
                .ok_or(ExternalReconciliationValidationError::ArithmeticOverflow)?;
        }
        if path_count > MAX_EXTERNAL_RECONCILIATION_PATHS {
            return Err(ExternalReconciliationValidationError::PathCountExceeded);
        }
        if path_bytes > MAX_EXTERNAL_RECONCILIATION_PATH_BYTES {
            return Err(ExternalReconciliationValidationError::PathBytesExceeded);
        }
        if metadata_bytes > MAX_EXTERNAL_RECONCILIATION_METADATA_BYTES {
            return Err(ExternalReconciliationValidationError::MetadataBytesExceeded);
        }
        Ok(ExternalRawDurableAccounting {
            event_count: self.observations.len() as u64,
            path_bytes: path_bytes as u64,
            metadata_bytes: metadata_bytes as u64,
        })
    }
}

impl ExternalRawObservationLimits {
    fn validate(&self) -> Result<(), ExternalReconciliationValidationError> {
        if self.max_events == 0 {
            Err(ExternalReconciliationValidationError::ZeroNativeEventLimit)
        } else {
            Ok(())
        }
    }
}

/// V15 external filesystem reconciliation command arguments.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalReconciliationIntentArgs {
    evidence_batch_id: EvidenceBatchId,
    source_participant_id: ParticipantId,
    filesystem_participant_id: ParticipantId,
    source_revision: u64,
    raw: ExternalRawObservationEnvelope,
}

impl ExternalReconciliationIntentArgs {
    /// Construct one external reconciliation observation from a durable raw envelope.
    pub fn new(
        evidence_batch_id: EvidenceBatchId,
        source_participant_id: ParticipantId,
        filesystem_participant_id: ParticipantId,
        source_revision: u64,
        raw: ExternalRawObservationEnvelope,
    ) -> Result<Self, ExternalReconciliationValidationError> {
        let args = Self {
            evidence_batch_id,
            source_participant_id,
            filesystem_participant_id,
            source_revision,
            raw,
        };
        args.validate()?;
        Ok(args)
    }

    /// Convert a borrowed native raw envelope into V15 arguments.
    pub fn from_raw_envelope(
        evidence_batch_id: EvidenceBatchId,
        source_participant_id: ParticipantId,
        filesystem_participant_id: ParticipantId,
        source_revision: u64,
        raw: &RawObservationEnvelope,
    ) -> Result<Self, ExternalReconciliationValidationError> {
        Self::new(
            evidence_batch_id,
            source_participant_id,
            filesystem_participant_id,
            source_revision,
            ExternalRawObservationEnvelope::from_raw_envelope(raw)?,
        )
    }

    /// Validate local bounds and the nonnil capture batch identity.
    pub fn validate(&self) -> Result<(), ExternalReconciliationValidationError> {
        self.evidence_batch_id.validate()?;
        self.raw.validate()
    }

    /// Validate source fence binding and required pending participants.
    pub fn validate_against_participants(
        &self,
        participants: &ParticipantSet,
    ) -> Result<(), ExternalReconciliationValidationError> {
        self.validate()?;
        participants.validate_for_new_intent()?;
        let source = required_source_descriptor(participants, self.source_participant_id)?;
        let source_fence = source
            .source_fence()
            .ok_or(ExternalReconciliationValidationError::SourceFenceMissing)?;
        if source_fence.source_id() != self.raw.provenance.source_id() {
            return Err(ExternalReconciliationValidationError::SourceIdentityMismatch);
        }
        if source_fence.source_revision() != self.source_revision {
            return Err(ExternalReconciliationValidationError::SourceRevisionMismatch);
        }
        required_filesystem_descriptor(participants, self.filesystem_participant_id)?;
        Ok(())
    }

    /// Return the capture batch identity, distinct from the operation id.
    pub const fn evidence_batch_id(&self) -> EvidenceBatchId {
        self.evidence_batch_id
    }

    /// Return the source-database participant binding.
    pub const fn source_participant_id(&self) -> ParticipantId {
        self.source_participant_id
    }

    /// Return the required filesystem participant binding.
    pub const fn filesystem_participant_id(&self) -> ParticipantId {
        self.filesystem_participant_id
    }

    /// Return the fenced source revision captured with the batch.
    pub const fn source_revision(&self) -> u64 {
        self.source_revision
    }

    /// Borrow the durable raw evidence.
    pub fn raw(&self) -> &ExternalRawObservationEnvelope {
        &self.raw
    }

    /// Return the conservative recovered-boundary audit requirement.
    pub const fn audit_requirement(&self) -> ExternalReconciliationAuditRequirement {
        ExternalReconciliationAuditRequirement::SourceAuditRequired
    }

    /// Return the external reconciliation universal command kind.
    pub const fn command_kind(&self) -> UniversalCommandKind {
        UniversalCommandKind::ExternalReconciliation
    }
}

fn required_source_descriptor(
    participants: &ParticipantSet,
    participant_id: ParticipantId,
) -> Result<&ParticipantDescriptor, ExternalReconciliationValidationError> {
    let descriptor = participants
        .descriptors()
        .iter()
        .find(|descriptor| descriptor.id() == participant_id)
        .ok_or(ExternalReconciliationValidationError::SourceParticipantMissing)?;
    if descriptor.kind() != ParticipantKind::SourceDatabase
        || descriptor.requirement() != ParticipantRequirement::Required
    {
        return Err(ExternalReconciliationValidationError::SourceParticipantWrongKind);
    }
    Ok(descriptor)
}

fn required_filesystem_descriptor(
    participants: &ParticipantSet,
    participant_id: ParticipantId,
) -> Result<&ParticipantDescriptor, ExternalReconciliationValidationError> {
    let descriptor = participants
        .descriptors()
        .iter()
        .find(|descriptor| descriptor.id() == participant_id)
        .ok_or(ExternalReconciliationValidationError::FilesystemParticipantMissing)?;
    if descriptor.kind() != ParticipantKind::Filesystem
        || descriptor.requirement() != ParticipantRequirement::Required
    {
        return Err(ExternalReconciliationValidationError::FilesystemParticipantWrongKind);
    }
    Ok(descriptor)
}

fn validate_source_id(source_id: &SourceId) -> Result<(), ExternalReconciliationValidationError> {
    if source_id.as_str().is_empty()
        || source_id.as_str().len() > MAX_SOURCE_IDENTITY_BYTES
        || source_id.as_str().chars().any(char::is_control)
    {
        Err(ExternalReconciliationValidationError::InvalidSourceIdentity)
    } else {
        Ok(())
    }
}

fn native_os_str_len(value: &OsStr) -> Result<usize, ExternalReconciliationValidationError> {
    #[cfg(unix)]
    {
        return Ok(value.as_bytes().len());
    }
    #[cfg(windows)]
    {
        return value
            .encode_wide()
            .count()
            .checked_mul(2)
            .ok_or(ExternalReconciliationValidationError::ArithmeticOverflow);
    }
    #[allow(unreachable_code)]
    Err(ExternalReconciliationValidationError::UnsupportedHostPlatform)
}

fn raw_observation_durable_metadata_bytes(
    observation: &RawObservation,
) -> Result<usize, ExternalReconciliationValidationError> {
    let mut bytes = 1usize
        .checked_add(std::mem::size_of::<u16>())
        .ok_or(ExternalReconciliationValidationError::ArithmeticOverflow)?;
    bytes = bytes
        .checked_add(raw_metadata_bytes(observation.metadata())?)
        .ok_or(ExternalReconciliationValidationError::ArithmeticOverflow)?;
    for path in observation.paths() {
        bytes = bytes
            .checked_add(1 + usize::from(path.hint().is_some()))
            .ok_or(ExternalReconciliationValidationError::ArithmeticOverflow)?;
    }
    Ok(bytes)
}

fn raw_metadata_bytes(
    metadata: &RawObservationMetadata,
) -> Result<usize, ExternalReconciliationValidationError> {
    let mut bytes = if metadata.flags() == 0 {
        0
    } else {
        std::mem::size_of::<u64>()
    };
    for present in [metadata.rename_cookie(), metadata.event_id()] {
        if present.is_some() {
            bytes = bytes
                .checked_add(std::mem::size_of::<u64>())
                .ok_or(ExternalReconciliationValidationError::ArithmeticOverflow)?;
        }
    }
    bytes = bytes
        .checked_add(metadata.cursor().map_or(0, <[u8]>::len))
        .ok_or(ExternalReconciliationValidationError::ArithmeticOverflow)?;
    for value in [metadata.detail(), metadata.source()] {
        if let Some(value) = value {
            bytes = bytes
                .checked_add(native_os_str_len(value)?)
                .ok_or(ExternalReconciliationValidationError::ArithmeticOverflow)?;
        }
    }
    if metadata.process_id().is_some() {
        bytes = bytes
            .checked_add(std::mem::size_of::<u32>())
            .ok_or(ExternalReconciliationValidationError::ArithmeticOverflow)?;
    }
    Ok(bytes)
}

fn raw_provenance_metadata_bytes(
    envelope: &RawObservationEnvelope,
) -> Result<usize, ExternalReconciliationValidationError> {
    let provenance = envelope.provenance();
    // Validate borrowed identities before any conversion can clone their backing bytes. The
    // native envelope is intentionally permissive, while the durable V15 envelope is bounded.
    validate_source_id(provenance.source_id())?;
    let root_identity = provenance
        .root_identity()
        .map(|identity| identity.as_bytes());
    let backend_stream_identity = provenance
        .backend_stream_identity()
        .map(|identity| identity.as_bytes());
    for identity in [root_identity, backend_stream_identity]
        .into_iter()
        .flatten()
    {
        if identity.len() > MAX_EXTERNAL_RECONCILIATION_IDENTITY_BYTES {
            return Err(ExternalReconciliationValidationError::IdentityTooLong);
        }
    }
    let mut bytes = provenance.source_id().as_str().len();
    bytes = bytes
        .checked_add(raw_provenance_metadata_bytes_without_source_parts(
            root_identity,
            backend_stream_identity,
        )?)
        .ok_or(ExternalReconciliationValidationError::ArithmeticOverflow)?;
    Ok(bytes)
}

fn raw_provenance_metadata_bytes_without_source(
    provenance: &ExternalRawObservationProvenance,
) -> Result<usize, ExternalReconciliationValidationError> {
    raw_provenance_metadata_bytes_without_source_parts(
        provenance.root_identity.as_deref(),
        provenance.backend_stream_identity.as_deref(),
    )
}

fn raw_provenance_metadata_bytes_without_source_parts(
    root_identity: Option<&[u8]>,
    backend_stream_identity: Option<&[u8]>,
) -> Result<usize, ExternalReconciliationValidationError> {
    let mut bytes = std::mem::size_of::<u64>() * 3;
    for identity in [root_identity, backend_stream_identity]
        .into_iter()
        .flatten()
    {
        bytes = bytes
            .checked_add(identity.len())
            .ok_or(ExternalReconciliationValidationError::ArithmeticOverflow)?;
    }
    bytes = bytes
        .checked_add(std::mem::size_of::<u64>() * 3)
        .ok_or(ExternalReconciliationValidationError::ArithmeticOverflow)?;
    Ok(bytes)
}

fn observation_durable_metadata_bytes(
    observation: &ExternalRawObservation,
) -> Result<usize, ExternalReconciliationValidationError> {
    let mut bytes = 1usize
        .checked_add(std::mem::size_of::<u16>())
        .ok_or(ExternalReconciliationValidationError::ArithmeticOverflow)?;
    bytes = bytes
        .checked_add(stored_metadata_bytes(&observation.metadata)?)
        .ok_or(ExternalReconciliationValidationError::ArithmeticOverflow)?;
    Ok(bytes)
}

fn stored_metadata_bytes(
    metadata: &ExternalRawObservationMetadata,
) -> Result<usize, ExternalReconciliationValidationError> {
    let mut bytes = if metadata.flags == 0 {
        0
    } else {
        std::mem::size_of::<u64>()
    };
    for present in [metadata.rename_cookie, metadata.event_id] {
        if present.is_some() {
            bytes = bytes
                .checked_add(std::mem::size_of::<u64>())
                .ok_or(ExternalReconciliationValidationError::ArithmeticOverflow)?;
        }
    }
    bytes = bytes
        .checked_add(metadata.cursor.as_ref().map_or(0, Vec::len))
        .ok_or(ExternalReconciliationValidationError::ArithmeticOverflow)?;
    for value in [&metadata.detail, &metadata.source].into_iter().flatten() {
        bytes = bytes
            .checked_add(value.durable_byte_len()?)
            .ok_or(ExternalReconciliationValidationError::ArithmeticOverflow)?;
    }
    if metadata.process_id.is_some() {
        bytes = bytes
            .checked_add(std::mem::size_of::<u32>())
            .ok_or(ExternalReconciliationValidationError::ArithmeticOverflow)?;
    }
    Ok(bytes)
}

/// Validate the strict V15 external command-arguments JSON boundary.
pub(crate) fn validate_persisted_external_reconciliation_intent_value(
    value: &Value,
) -> Result<(), &'static str> {
    validate_persisted_external_nullable_fields(value)?;
    let args = serde_json::from_value::<ExternalReconciliationIntentArgs>(value.clone())
        .map_err(|_| "persisted external reconciliation command arguments are malformed")?;
    args.validate().map_err(
        |_| "persisted external reconciliation command arguments violate their bounded contract",
    )
}

fn validate_persisted_external_nullable_fields(value: &Value) -> Result<(), &'static str> {
    let args = external_object(value)?;
    let raw = external_object(required_external_field(args, "raw")?)?;
    let provenance = external_object(required_external_field(raw, "provenance")?)?;
    for field in ["root_identity", "backend_stream_identity"] {
        required_external_field(provenance, field)?;
    }
    let boundary = external_object(required_external_field(provenance, "capture_boundary")?)?;
    for field in ["first_sequence", "last_sequence"] {
        required_external_field(boundary, field)?;
    }

    let observations = required_external_field(raw, "observations")?
        .as_array()
        .ok_or("persisted external observations must be an array")?;
    for observation in observations {
        let observation = external_object(observation)?;
        let paths = required_external_field(observation, "paths")?
            .as_array()
            .ok_or("persisted external observation paths must be an array")?;
        for path in paths {
            let path = external_object(path)?;
            required_external_field(path, "hint")?;
        }

        let metadata = external_object(required_external_field(observation, "metadata")?)?;
        for field in [
            "rename_cookie",
            "event_id",
            "cursor",
            "detail",
            "source",
            "process_id",
        ] {
            required_external_field(metadata, field)?;
        }
    }
    Ok(())
}

fn external_object(value: &Value) -> Result<&Map<String, Value>, &'static str> {
    value
        .as_object()
        .ok_or("persisted external nested value must be a JSON object")
}

fn required_external_field<'a>(
    object: &'a Map<String, Value>,
    field: &str,
) -> Result<&'a Value, &'static str> {
    object
        .get(field)
        .ok_or("persisted external nullable field is missing")
}

/// Why one external reconciliation observation failed its bounded contract.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum ExternalReconciliationValidationError {
    /// The capture batch UUID was nil.
    #[error("external evidence batch UUID must be non-nil")]
    NilEvidenceBatch,
    /// A source ID was empty, oversized, or contained controls.
    #[error("external source identity is invalid")]
    InvalidSourceIdentity,
    /// A native identity exceeded its bound.
    #[error("external opaque identity exceeds its bound")]
    IdentityTooLong,
    /// A native string exceeded its bound.
    #[error("external native string exceeds its bound")]
    NativeStringTooLong,
    /// The source host cannot provide a supported lossless native spelling.
    #[error("external native string is unsupported on this host platform")]
    UnsupportedHostPlatform,
    /// A stored native string belongs to another platform.
    #[error("external native string belongs to a foreign platform")]
    ForeignPlatformString,
    /// A checked arithmetic operation overflowed.
    #[error("external reconciliation accounting overflowed")]
    ArithmeticOverflow,
    /// Capture sequence endpoints were reversed.
    #[error("external capture sequence boundaries are not ordered")]
    InvalidCaptureBoundary,
    /// The original native event budget was zero.
    #[error("external native event limit is zero")]
    ZeroNativeEventLimit,
    /// The durable event bound was exceeded.
    #[error("external reconciliation event count exceeds its bound")]
    EventCountExceeded,
    /// The durable path count bound was exceeded.
    #[error("external reconciliation path count exceeds its bound")]
    PathCountExceeded,
    /// The durable path-byte bound was exceeded.
    #[error("external reconciliation path bytes exceed their bound")]
    PathBytesExceeded,
    /// The durable metadata-byte bound was exceeded.
    #[error("external reconciliation metadata exceeds its bound")]
    MetadataBytesExceeded,
    /// Durable accounting did not match the retained observations.
    #[error("external durable accounting does not match retained observations")]
    DurableAccountingMismatch,
    /// Native event accounting did not match retained event count.
    #[error("external native event accounting does not match retained observations")]
    NativeAccountingMismatch,
    /// The source participant was absent.
    #[error("external reconciliation source participant is missing")]
    SourceParticipantMissing,
    /// The source participant was not a required SourceDatabase.
    #[error("external reconciliation source participant must be a required SourceDatabase")]
    SourceParticipantWrongKind,
    /// The source participant did not carry a fence.
    #[error("external reconciliation source participant has no source fence")]
    SourceFenceMissing,
    /// The observed source ID did not match its fence.
    #[error("external reconciliation source identity does not match its fence")]
    SourceIdentityMismatch,
    /// The observed source revision did not match its fence.
    #[error("external reconciliation source revision does not match its fence")]
    SourceRevisionMismatch,
    /// The filesystem participant was absent.
    #[error("external reconciliation filesystem participant is missing")]
    FilesystemParticipantMissing,
    /// The filesystem participant was not required.
    #[error("external reconciliation filesystem participant must be required")]
    FilesystemParticipantWrongKind,
    /// The participant inventory failed its common validation.
    #[error("invalid external reconciliation participants: {0}")]
    Participants(#[from] ParticipantValidationError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operation_journal::{
        ParticipantCheckpoint, ParticipantCheckpointState, ParticipantRequirement,
        StoredSourceFence,
    };
    use crate::sample_sources::reconciliation::{
        CaptureBoundary, RawObservation, RawObservationEnvelope, RawObservationLimits,
        RawObservationMetadata, RawObservationProvenance, RawObservedPath, RawPathRole,
        RootIdentity, WatcherGeneration,
    };

    fn participant_fixture() -> (ParticipantSet, ParticipantId, ParticipantId, SourceId) {
        let source_participant = ParticipantId::from_uuid(Uuid::from_u128(1));
        let filesystem_participant = ParticipantId::from_uuid(Uuid::from_u128(2));
        let source_id = SourceId::from_string("source-a");
        let source_fence =
            StoredSourceFence::new(source_id.clone(), "root-a", "database-a", 3, 9, 1)
                .expect("source fence");
        let participants = ParticipantSet::new(
            vec![
                ParticipantDescriptor::new(
                    source_participant,
                    ParticipantKind::SourceDatabase,
                    ParticipantRequirement::Required,
                    Some(source_fence),
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
        (
            participants,
            source_participant,
            filesystem_participant,
            source_id,
        )
    }

    fn stored_marker(source_id: SourceId) -> ExternalRawObservationEnvelope {
        let provenance = ExternalRawObservationProvenance::new(
            source_id,
            Some(vec![1, 2, 3]),
            None,
            8,
            ExternalCaptureBoundary::new(55, None, Some(4)).expect("boundary"),
        )
        .expect("provenance");
        let limits = ExternalRawObservationLimits::new(64, 4096, 4096).expect("limits");
        let native = ExternalRawNativeAccounting::new(1, 0, 1);
        let marker = ExternalRawObservation::new(
            ExternalRawEventKind::Overflow,
            Vec::new(),
            ExternalRawObservationMetadata::new(),
            1 << 15,
        )
        .expect("marker");
        ExternalRawObservationEnvelope::new(provenance, limits, native, vec![marker])
            .expect("envelope")
    }

    #[cfg(unix)]
    #[test]
    fn borrowed_conversion_preserves_non_utf8_duplicates_limits_and_unknown_bits() {
        let source_id = SourceId::from_string("source-a");
        let provenance = RawObservationProvenance::new(
            source_id,
            Some(RootIdentity::from_bytes(vec![1, 2])),
            None,
            WatcherGeneration::new(9),
            CaptureBoundary::try_new(44, Some(7), None).expect("boundary"),
        );
        let path = std::path::PathBuf::from(std::ffi::OsString::from_vec(vec![b'f', b'o', 0xff]));
        let observation = RawObservation::new(
            RawEventKind::Modify,
            vec![
                RawObservedPath::new(path.clone(), RawPathRole::Subject),
                RawObservedPath::new(path, RawPathRole::Subject),
            ],
        )
        .with_metadata(RawObservationMetadata::new().with_cursor(vec![9, 8]));
        let observation = observation.with_uncertainty(
            crate::sample_sources::reconciliation::ObservationUncertainty::from_bits(1 << 15),
        );
        let raw = RawObservationEnvelope::try_new(
            provenance,
            vec![observation],
            RawObservationLimits::new(64, 4096, 4096).expect("limits"),
        )
        .expect("raw envelope");
        let stored = ExternalRawObservationEnvelope::from_raw_envelope(&raw).expect("stored");
        assert_eq!(stored.observations()[0].paths().len(), 2);
        assert_eq!(
            stored.observations()[0].paths()[0].path().unix_bytes_ref(),
            Some(&b"fo\xff"[..])
        );
        assert_eq!(stored.observations()[0].uncertainty_bits(), 1 << 15);
        assert_eq!(
            stored.provenance().capture_boundary().sequence_evidence(),
            ExternalCaptureSequenceEvidence::Ambiguous
        );
        assert_eq!(stored.limits().max_native_path_bytes(), 4096);
        assert_eq!(
            stored.native_accounting().path_bytes(),
            raw.accounting().path_bytes() as u64
        );
        assert!(stored.durable_accounting().path_bytes() > 0);
    }

    #[test]
    fn borrowed_conversion_rejects_oversized_provenance_before_materializing_it() {
        let provenance = RawObservationProvenance::new(
            SourceId::from_string("source-a"),
            Some(RootIdentity::from_bytes(vec![
                0;
                MAX_EXTERNAL_RECONCILIATION_IDENTITY_BYTES
                    + 1
            ])),
            None,
            WatcherGeneration::new(1),
            CaptureBoundary::try_new(1, None, None).expect("boundary"),
        );
        let raw = RawObservationEnvelope::try_new(
            provenance,
            vec![RawObservation::new(RawEventKind::Overflow, Vec::new())],
            RawObservationLimits::new(1, 0, 4096).expect("limits"),
        )
        .expect("raw envelope");

        assert_eq!(
            ExternalRawObservationEnvelope::from_raw_envelope(&raw),
            Err(ExternalReconciliationValidationError::IdentityTooLong)
        );
    }

    #[test]
    fn foreign_windows_units_are_lossless_but_not_reconstructed_on_unix() {
        let value = ExternalNativeString::windows_utf16(vec![0x61, 0xd800]).expect("units");
        assert_eq!(value.windows_utf16_ref(), Some(&[0x61, 0xd800][..]));
        #[cfg(unix)]
        assert_eq!(
            value.try_to_os_string(),
            Err(ExternalReconciliationValidationError::ForeignPlatformString)
        );
    }

    #[test]
    fn external_args_bind_source_revision_and_required_filesystem() {
        let (participants, source_participant, filesystem_participant, source_id) =
            participant_fixture();
        let args = ExternalReconciliationIntentArgs::new(
            EvidenceBatchId::from_uuid(Uuid::from_u128(42)),
            source_participant,
            filesystem_participant,
            9,
            stored_marker(source_id),
        )
        .expect("external args");
        args.validate_against_participants(&participants)
            .expect("participant binding");
        assert_eq!(
            args.audit_requirement(),
            ExternalReconciliationAuditRequirement::SourceAuditRequired
        );
        assert_eq!(
            args.command_kind(),
            UniversalCommandKind::ExternalReconciliation
        );
    }

    #[test]
    fn external_args_reject_nil_batch_and_source_revision_mismatch() {
        let (participants, source_participant, filesystem_participant, source_id) =
            participant_fixture();
        assert_eq!(
            ExternalReconciliationIntentArgs::new(
                EvidenceBatchId::from_uuid(Uuid::nil()),
                source_participant,
                filesystem_participant,
                9,
                stored_marker(source_id.clone()),
            ),
            Err(ExternalReconciliationValidationError::NilEvidenceBatch)
        );
        let args = ExternalReconciliationIntentArgs::new(
            EvidenceBatchId::from_uuid(Uuid::from_u128(43)),
            source_participant,
            filesystem_participant,
            8,
            stored_marker(source_id),
        )
        .expect("external args");
        assert_eq!(
            args.validate_against_participants(&participants),
            Err(ExternalReconciliationValidationError::SourceRevisionMismatch)
        );
    }

    #[test]
    fn external_durable_bounds_reject_oversized_paths_and_metadata() {
        let provenance = ExternalRawObservationProvenance::new(
            SourceId::from_string("source-a"),
            None,
            None,
            1,
            ExternalCaptureBoundary::new(1, None, None).expect("boundary"),
        )
        .expect("provenance");
        let path = ExternalRawObservedPath::new(
            ExternalNativeString::unix_bytes(vec![7; MAX_EXTERNAL_RECONCILIATION_PATH_BYTES + 1])
                .expect("path spelling"),
            ExternalRawPathRole::Subject,
            None,
        );
        assert_eq!(
            path,
            Err(ExternalReconciliationValidationError::NativeStringTooLong)
        );
        let metadata = ExternalRawObservationMetadata::new()
            .with_cursor(vec![1; MAX_EXTERNAL_RECONCILIATION_METADATA_BYTES]);
        let observation =
            ExternalRawObservation::new(ExternalRawEventKind::Modify, Vec::new(), metadata, 0)
                .expect("observation");
        let limits = ExternalRawObservationLimits::new(64, 4096, 4096).expect("limits");
        let native = ExternalRawNativeAccounting::new(1, 0, 0);
        assert_eq!(
            ExternalRawObservationEnvelope::new(provenance, limits, native, vec![observation]),
            Err(ExternalReconciliationValidationError::MetadataBytesExceeded)
        );
    }

    #[test]
    fn persisted_external_nullable_fields_require_explicit_nulls() {
        let (_participants, source_participant, filesystem_participant, source_id) =
            participant_fixture();
        let provenance = ExternalRawObservationProvenance::new(
            source_id,
            None,
            None,
            1,
            ExternalCaptureBoundary::new(1, None, None).expect("boundary"),
        )
        .expect("provenance");
        let path = ExternalRawObservedPath::new(
            ExternalNativeString::unix_bytes(b"observed.wav".to_vec()).expect("path spelling"),
            ExternalRawPathRole::Subject,
            None,
        )
        .expect("path");
        let observation = ExternalRawObservation::new(
            ExternalRawEventKind::Modify,
            vec![path],
            ExternalRawObservationMetadata::new(),
            0,
        )
        .expect("observation");
        let raw = ExternalRawObservationEnvelope::new(
            provenance,
            ExternalRawObservationLimits::new(64, 4096, 4096).expect("limits"),
            ExternalRawNativeAccounting::new(1, 12, 4),
            vec![observation],
        )
        .expect("envelope");
        let args = ExternalReconciliationIntentArgs::new(
            EvidenceBatchId::from_uuid(Uuid::from_u128(42)),
            source_participant,
            filesystem_participant,
            9,
            raw,
        )
        .expect("arguments");
        let encoded = serde_json::to_value(&args).expect("arguments JSON");
        validate_persisted_external_reconciliation_intent_value(&encoded)
            .expect("explicit nullable fields");
        let decoded: ExternalReconciliationIntentArgs =
            serde_json::from_value(encoded.clone()).expect("explicit null decode");
        assert_eq!(serde_json::to_value(decoded).expect("re-encode"), encoded);

        for field_path in [
            &["raw", "provenance", "root_identity"][..],
            &["raw", "provenance", "backend_stream_identity"][..],
            &["raw", "provenance", "capture_boundary", "first_sequence"][..],
            &["raw", "provenance", "capture_boundary", "last_sequence"][..],
            &["raw", "observations", "0", "metadata", "rename_cookie"][..],
            &["raw", "observations", "0", "metadata", "event_id"][..],
            &["raw", "observations", "0", "metadata", "cursor"][..],
            &["raw", "observations", "0", "metadata", "detail"][..],
            &["raw", "observations", "0", "metadata", "source"][..],
            &["raw", "observations", "0", "metadata", "process_id"][..],
        ] {
            let mut missing = encoded.clone();
            let mut current = &mut missing;
            for component in &field_path[..field_path.len() - 1] {
                current = if let Ok(index) = component.parse::<usize>() {
                    current
                        .as_array_mut()
                        .expect("array component")
                        .get_mut(index)
                        .expect("array index")
                } else {
                    current
                        .as_object_mut()
                        .expect("object component")
                        .get_mut(*component)
                        .expect("object field")
                };
            }
            let field = field_path.last().expect("field path");
            if let Ok(index) = field.parse::<usize>() {
                current.as_array_mut().expect("array field").remove(index);
            } else {
                current
                    .as_object_mut()
                    .expect("object field")
                    .remove(*field)
                    .expect("nullable field");
            }
            assert!(
                validate_persisted_external_reconciliation_intent_value(&missing).is_err(),
                "missing nullable field {field_path:?} was accepted"
            );
        }

        let mut missing_hint = encoded;
        missing_hint["raw"]["observations"][0]["paths"][0]
            .as_object_mut()
            .expect("path object")
            .remove("hint")
            .expect("path hint");
        assert!(validate_persisted_external_reconciliation_intent_value(&missing_hint).is_err());
    }
}
