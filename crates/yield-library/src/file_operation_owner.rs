//! Bounded ownership for future file-operation effects.
//!
//! This module owns only an isolated command queue, root-registration observations, and
//! bounded effect receipts.  It deliberately does not open files, hold SQLite connections,
//! retain source leases, or manufacture live authority.  Until the producer and backend
//! adapters land, accepted commands finish as typed [`EffectOutcome::Unsupported`] results.
//! The test-only fixtures exercise the owner boundary without pretending to qualify a
//! filesystem effect.

use std::collections::BTreeMap;
use std::fmt;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Component, Path};
#[cfg(test)]
use std::sync::Condvar;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::Serialize;
use thiserror::Error;
use uuid::Uuid;

use crate::operation_journal::owner::LiveChunkEffectGrant;
use crate::operation_journal::{MAX_RECORD_BYTES, OperationId, ParticipantId};
use crate::sample_sources::SourceId;

/// Maximum number of effect commands waiting for the file owner.
pub const FILE_OPERATION_OWNER_COMMAND_CAPACITY: usize = 32;
/// Maximum number of result handles that may remain outstanding.
pub const FILE_OPERATION_OWNER_RESULT_CAPACITY: usize = 64;
/// Maximum encoded input retained across queued and executing commands.
pub const FILE_OPERATION_OWNER_MAX_INPUT_BYTES: usize = MAX_RECORD_BYTES * 4;
/// Maximum number of retained unacknowledged effect receipts.
pub const FILE_OPERATION_OWNER_MAX_RECEIPTS: usize = 64;
/// Maximum number of roots used by one primitive.
pub const FILE_OPERATION_OWNER_MAX_ROOTS: usize = 2;
/// Maximum number of source fences used by one primitive.
pub const FILE_OPERATION_OWNER_MAX_SOURCE_FENCES: usize = 2;
/// Maximum byte length of one relative locator.
pub const FILE_OPERATION_OWNER_MAX_LOCATOR_BYTES: usize = 1024;
/// Maximum byte length of one owner diagnostic.
pub const FILE_OPERATION_OWNER_MAX_DIAGNOSTIC_BYTES: usize = 512;

/// Opaque identity of the owner that retains a registration or receipt.
///
/// The identity is an observation used to reject a command sent to the wrong owner.  It is not
/// a root capability and does not permit a caller to create a registration.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct OwnerIdentity(Uuid);

/// Stable identity for one file-owner effect.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct EffectId(Uuid);

impl EffectId {
    /// Generate an effect identity.  This identifies work; it grants no filesystem authority.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Rehydrate an effect identity without granting authority.
    pub fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    /// Borrow the UUID representation of this observation.
    pub fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for EffectId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable identity for one output or artifact bound to a write effect.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct OutputIdentity(Uuid);

impl OutputIdentity {
    /// Generate an output observation identity.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Rehydrate an output observation identity.
    pub fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    /// Borrow the UUID representation of this observation.
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for OutputIdentity {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable chunk identity echoed by the bounded write request.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ChunkId(Uuid);

impl ChunkId {
    /// Generate a chunk identity observation.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Rehydrate a chunk observation without granting write authority.
    pub fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    /// Borrow the UUID representation of this observation.
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for ChunkId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable issuance identity echoed by the bounded write request.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct IssueId(Uuid);

impl IssueId {
    /// Generate an issuance identity observation.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Rehydrate an issue observation without granting write authority.
    pub fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    /// Borrow the UUID representation of this observation.
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for IssueId {
    fn default() -> Self {
        Self::new()
    }
}

/// Validation failure for bounded, authority-free request values.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("invalid file-operation owner value: {message}")]
pub struct FileOperationValidationError {
    message: String,
}

impl FileOperationValidationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: bounded_diagnostic(message.into()),
        }
    }
}

/// A source lifecycle fence copied from a currently validated source owner.
///
/// This value is an observation.  The file owner cannot use it to acquire or renew a source
/// lease; a future sealed live authorization must still be supplied for mutating effects.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct SourceFence {
    source_id: SourceId,
    root_identity: String,
    database_identity: String,
    lifecycle_generation: u64,
    source_revision: Option<u64>,
    lease_epoch: u64,
}

impl SourceFence {
    /// Build a bounded source-fence observation.
    pub fn new(
        source_id: SourceId,
        root_identity: impl Into<String>,
        database_identity: impl Into<String>,
        lifecycle_generation: u64,
        source_revision: Option<u64>,
        lease_epoch: u64,
    ) -> Result<Self, FileOperationValidationError> {
        let value = Self {
            source_id: SourceId::from_string(normalize_string(source_id.as_str().to_owned())),
            root_identity: normalize_string(root_identity.into()),
            database_identity: normalize_string(database_identity.into()),
            lifecycle_generation,
            source_revision,
            lease_epoch,
        };
        value.validate()?;
        Ok(value)
    }

    /// Validate the fence as a bounded observation.
    pub fn validate(&self) -> Result<(), FileOperationValidationError> {
        validate_text("source identity", self.source_id.as_str(), 256)?;
        validate_text("source root identity", &self.root_identity, 256)?;
        validate_text("source database identity", &self.database_identity, 256)?;
        if self.lease_epoch == 0 {
            return Err(FileOperationValidationError::new(
                "lease epoch must be positive",
            ));
        }
        Ok(())
    }

    /// Return the heap-backed bytes retained by this normalized fence.
    ///
    /// `SourceFence::new` normalizes the `SourceId` string before storing it, so its length is
    /// also its allocation bound.  The two ordinary `String` fields expose their capacities
    /// directly.  This is used only for the owner input budget; it never enters the request
    /// fingerprint or grants authority.
    pub(crate) fn dynamic_backing_bytes(&self) -> Option<usize> {
        self.source_id
            .as_str()
            .len()
            .checked_add(self.root_identity.capacity())?
            .checked_add(self.database_identity.capacity())
    }

    /// Return the source identifier.
    pub fn source_id(&self) -> &SourceId {
        &self.source_id
    }

    /// Return the observed source-root identity without granting root access.
    pub(crate) fn root_identity(&self) -> &str {
        &self.root_identity
    }

    /// Return the observed source database identity without granting database access.
    pub(crate) fn database_identity(&self) -> &str {
        &self.database_identity
    }

    /// Return the source lifecycle generation.
    pub const fn lifecycle_generation(&self) -> u64 {
        self.lifecycle_generation
    }

    /// Return the optional source revision.
    pub const fn source_revision(&self) -> Option<u64> {
        self.source_revision
    }

    /// Return the positive lease epoch.
    pub const fn lease_epoch(&self) -> u64 {
        self.lease_epoch
    }
}

/// Opaque relative location below an owner-registered root.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct BoundedRelativeLocator(String);

impl BoundedRelativeLocator {
    /// Validate and retain a relative locator.  Absolute paths and parent traversal are rejected.
    pub fn new(value: impl Into<String>) -> Result<Self, FileOperationValidationError> {
        let value = value.into();
        validate_text(
            "relative locator",
            &value,
            FILE_OPERATION_OWNER_MAX_LOCATOR_BYTES,
        )?;
        let path = Path::new(&value);
        if path.is_absolute()
            || path.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(FileOperationValidationError::new(
                "relative locator must stay below its retained root",
            ));
        }
        if value == "."
            || value.is_empty()
            || value
                .split(['/', '\\'])
                .any(|component| component.is_empty() || component == ".")
        {
            return Err(FileOperationValidationError::new(
                "relative locator must identify a bounded entry",
            ));
        }
        Ok(Self(normalize_string(value)))
    }

    /// Borrow the validated relative locator.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn validate(&self) -> Result<(), FileOperationValidationError> {
        Self::new(self.0.clone()).map(|_| ())
    }
}

/// Stable identity observed for a filesystem object.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct OwnedObjectIdentity(Uuid);

impl OwnedObjectIdentity {
    /// Generate an object identity for a test or future qualified backend.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for OwnedObjectIdentity {
    fn default() -> Self {
        Self::new()
    }
}

/// Exact object or absence predicate used by an effect boundary.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
pub enum ExpectedObject {
    /// The effect requires the target entry to be absent.
    Absent,
    /// The effect requires this exact observed object identity.
    Exact(OwnedObjectIdentity),
}

/// A bounded positive byte count used by an effect request.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct PositiveBoundedBytes(u64);

impl PositiveBoundedBytes {
    /// Validate a positive byte count within the owner input bound.
    pub fn new(value: u64) -> Result<Self, FileOperationValidationError> {
        if value == 0 || value > MAX_RECORD_BYTES as u64 {
            return Err(FileOperationValidationError::new(
                "byte count must be positive and fit the owner record bound",
            ));
        }
        Ok(Self(value))
    }

    /// Return the checked byte count.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A bounded payload retained by an accepted owner command.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct BoundedBytes(Box<[u8]>);

impl BoundedBytes {
    /// Validate and retain a payload no larger than the owner record bound.
    pub fn new(value: Vec<u8>) -> Result<Self, FileOperationValidationError> {
        if value.len() > MAX_RECORD_BYTES {
            return Err(FileOperationValidationError::new(
                "payload exceeds the owner record bound",
            ));
        }
        Ok(Self(value.into_boxed_slice()))
    }

    /// Borrow the retained payload bytes.
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

/// A checked byte offset for a bounded write.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct BoundedOffset(u64);

impl BoundedOffset {
    /// Validate a byte offset.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Return the byte offset.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A checked range used by a source copy.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct BoundedRange {
    /// Start offset in the source object.
    pub offset: BoundedOffset,
    /// Positive bounded range length.
    pub length: PositiveBoundedBytes,
}

impl BoundedRange {
    /// Validate that the range does not overflow.
    pub fn new(
        offset: BoundedOffset,
        length: PositiveBoundedBytes,
    ) -> Result<Self, FileOperationValidationError> {
        offset
            .get()
            .checked_add(length.get())
            .ok_or_else(|| FileOperationValidationError::new("range offset overflows"))?;
        Ok(Self { offset, length })
    }
}

/// A profile-owned authority fence observation for a registered root.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct ProfileAuthorityFence {
    owner: OwnerIdentity,
    generation: u64,
    lease_epoch: u64,
}

/// Authority fence associated with one registered root role.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub enum AuthorityFence {
    /// A source-owned lifecycle fence.
    Source(SourceFence),
    /// A profile-owned generation and positive lease epoch.
    Profile(ProfileAuthorityFence),
}

/// Opaque owner-local root registration identity and generation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct RootRegistrationRef {
    owner: OwnerIdentity,
    registration_id: Uuid,
    generation: u64,
}

impl RootRegistrationRef {
    #[cfg(test)]
    pub(crate) fn synthetic_for_test() -> Self {
        Self {
            owner: OwnerIdentity(Uuid::new_v4()),
            registration_id: Uuid::new_v4(),
            generation: 1,
        }
    }

    /// Return the owning file-operation owner identity as an observation.
    pub const fn owner_identity(self) -> OwnerIdentity {
        self.owner
    }

    /// Return the registration UUID as an observation.
    pub const fn registration_id(self) -> Uuid {
        self.registration_id
    }

    /// Return the registration generation.
    pub const fn generation(self) -> u64 {
        self.generation
    }

    #[cfg(test)]
    fn next_generation(self) -> Result<Self, FileOperationValidationError> {
        let generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| FileOperationValidationError::new("root generation overflows"))?;
        Ok(Self { generation, ..self })
    }
}

/// Root role echoed by an effect request and its authority-free receipt.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
pub enum RootRole {
    /// A source root used for a read or move source.
    Source,
    /// A destination root used for a new namespace entry or final target.
    Destination,
    /// A retained staging root.
    Staging,
    /// A root used only for observation or cleanup.
    Owned,
}

/// A role binding carried by one effect payload.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct RootRoleBinding {
    registration: RootRegistrationRef,
    authority: AuthorityFence,
    locator: BoundedRelativeLocator,
    expected: ExpectedObject,
}

impl RootRoleBinding {
    /// Return the opaque owner-local registration reference.
    pub const fn registration(&self) -> RootRegistrationRef {
        self.registration
    }

    /// Return the authority fence observation.
    pub fn authority(&self) -> &AuthorityFence {
        &self.authority
    }

    /// Return the bounded locator.
    pub fn locator(&self) -> &BoundedRelativeLocator {
        &self.locator
    }

    /// Return the exact expected object predicate.
    pub const fn expected(&self) -> ExpectedObject {
        self.expected
    }
}

/// A source role that must carry a source authority fence.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct SourceRoot {
    binding: RootRoleBinding,
}

/// A destination role for staging or publication.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct DestinationRoot {
    binding: RootRoleBinding,
}

/// A retained staging role.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct StagingRoot {
    binding: RootRoleBinding,
}

/// A generic owned root used by observation and cleanup effects.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct OwnedRoot {
    binding: RootRoleBinding,
}

/// Evidence that an effect has validated staged content.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct CopyValidatedEvidence {
    object: OwnedObjectIdentity,
    bytes: u64,
}

/// Evidence needed by a qualified cleanup primitive.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct CleanupProof {
    marker: u64,
}

/// Evidence needed by a qualified restore primitive.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct RestoreProof {
    marker: u64,
}

/// Staged-content validation requirement.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct StagingValidation {
    expected_bytes: u64,
}

/// All effect payloads understood by the owner boundary.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub enum EffectRequest {
    /// Create an absent staging object below a destination root.
    CreateStaging {
        /// Destination/staging root role.
        destination: DestinationRoot,
        /// Explicit absence proof required before creation.
        expected_absent: AbsenceProof,
        /// Positive bounded staging size.
        bytes: PositiveBoundedBytes,
    },
    /// Copy a bounded source range into retained staging.
    CopyIntoStaging {
        /// Independently bound source root.
        source: SourceRoot,
        /// Destination staging root.
        staging: StagingRoot,
        /// Bounded source range.
        source_range: BoundedRange,
        /// Positive bounded destination allocation.
        bytes: PositiveBoundedBytes,
    },
    /// Write a bounded chunk into retained staging.
    WriteStagingChunk {
        /// Retained staging root.
        staging: StagingRoot,
        /// Stable chunk identity.
        chunk_id: ChunkId,
        /// Stable issuance identity.
        issue_id: IssueId,
        /// Exact output/artifact identity.
        output: OutputIdentity,
        /// Checked logical write offset.
        offset: BoundedOffset,
        /// Positive bounded write length.
        length: PositiveBoundedBytes,
        /// Bounded payload bytes.
        payload: BoundedBytes,
    },
    /// Flush a retained staging object.
    FlushStaging {
        /// Retained staging root.
        staging: StagingRoot,
        /// Exact staged object identity.
        object: OwnedObjectIdentity,
    },
    /// Validate the retained staging object.
    ValidateStaging {
        /// Retained staging root.
        staging: StagingRoot,
        /// Exact staged object identity.
        object: OwnedObjectIdentity,
        /// Bounded validation requirement.
        expected: StagingValidation,
    },
    /// Publish staging with a final no-replace namespace claim.
    PublishNoReplace {
        /// Retained staging root.
        staging: StagingRoot,
        /// Destination root.
        target: DestinationRoot,
        /// Durable staged validation evidence.
        staged: CopyValidatedEvidence,
        /// Explicit target absence proof.
        expected_absent: AbsenceProof,
    },
    /// Move one exact source object to an absent destination.
    MoveNoReplace {
        /// Independently bound source root.
        source: SourceRoot,
        /// Destination root.
        destination: DestinationRoot,
        /// Exact source object identity.
        source_object: OwnedObjectIdentity,
        /// Explicit destination absence proof.
        expected_absent: AbsenceProof,
    },
    /// Create an absent directory below a destination root.
    CreateDirectoryNoReplace {
        /// Destination root.
        destination: DestinationRoot,
        /// Explicit absence proof.
        expected_absent: AbsenceProof,
    },
    /// Replace one exact expected target using validated staging.
    ReplaceExpectedTarget {
        /// Retained staging root.
        staging: StagingRoot,
        /// Destination root.
        target: DestinationRoot,
        /// Durable staged validation evidence.
        staged: CopyValidatedEvidence,
        /// Exact expected target identity.
        expected_target: OwnedObjectIdentity,
    },
    /// Inspect an object below an owned root without granting mutation authority.
    InspectOwnedObject {
        /// Owned root observation.
        root: OwnedRoot,
        /// Exact identity or absence predicate.
        expected: ExpectedObject,
    },
    /// Remove an exact object using qualified cleanup evidence.
    RemoveOwnedObject {
        /// Owned root observation.
        root: OwnedRoot,
        /// Exact object identity.
        expected: OwnedObjectIdentity,
        /// Qualified cleanup proof.
        cleanup: CleanupProof,
    },
    /// Restore an exact retained object using qualified restore evidence.
    RestoreOwnedObject {
        /// Owned root observation.
        root: OwnedRoot,
        /// Exact retained object identity.
        expected: OwnedObjectIdentity,
        /// Qualified restore proof.
        restore: RestoreProof,
    },
}

/// Explicit absence proof used by no-replace effect requests.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct AbsenceProof {
    marker: u64,
}

/// Sealed intent authority supplied by a future coordinator adapter.
///
/// No production constructor is exposed.  A test-only fixture can create a synthetic value to
/// exercise validation and queue behavior; it does not qualify a backend or source lease.
#[derive(Debug, Eq, PartialEq)]
pub struct LiveIntentAuthorization {
    owner: OwnerIdentity,
    nonce: u64,
    source_fences: Vec<SourceFence>,
}

/// Sealed capacity authority supplied by a future journal/capacity adapter.
///
/// This value is intentionally non-constructible by production callers in the current tree.
#[derive(Debug, Eq, PartialEq)]
pub struct LiveCapacityAuthorization {
    owner: OwnerIdentity,
    nonce: u64,
    available_bytes: u64,
    protected_floor: u64,
}

/// Marker for a command whose live authority is carried separately by a sealed journal grant.
#[derive(Debug, Eq, PartialEq)]
pub struct JournalGrantAuthorization {
    _sealed: (),
}

/// Authorization mode for one effect request.
#[derive(Debug, Eq, PartialEq)]
pub enum EffectAuthorization {
    /// A live intent and capacity proof for a mutating effect.
    Mutating {
        /// Sealed live intent proof.
        live_intent: LiveIntentAuthorization,
        /// Sealed live capacity proof.
        live_capacity: LiveCapacityAuthorization,
    },
    /// An authority-free command paired with a non-Clone journal grant on the owner queue.
    JournalGrant(JournalGrantAuthorization),
    /// A live root registration and source fences for observation only.
    ObservationOnly {
        /// Owner-local root registration.
        live_root_registration: RootRegistrationRef,
        /// Source fences required for the observation.
        source_fences: Vec<SourceFence>,
    },
}

/// Complete bounded request submitted to the file owner.
///
/// Fields are private so callers cannot manufacture a root registration, live authority, or
/// arbitrary filesystem path.  Producer adapters will receive dedicated sealed constructors in
/// a later ticket.
#[derive(Debug, Eq, PartialEq)]
pub struct FileOperationCommand {
    operation_id: OperationId,
    participant_id: ParticipantId,
    effect_id: EffectId,
    source_fences: Vec<SourceFence>,
    target_registration: RootRegistrationRef,
    expected: ExpectedObject,
    effect: EffectRequest,
    authorization: EffectAuthorization,
}

/// Authority-free request projection used for bounded canonical identity and duplicate checks.
///
/// Live intent/capacity proofs are deliberately absent.  A producer must be able to refresh
/// those proofs for an exact retry without changing the durable effect identity.
#[derive(Serialize)]
struct AuthorityFreeCommandView<'a> {
    operation_id: OperationId,
    participant_id: ParticipantId,
    effect_id: EffectId,
    source_fences: &'a [SourceFence],
    target_registration: RootRegistrationRef,
    expected: ExpectedObject,
    effect: &'a EffectRequest,
}

impl FileOperationCommand {
    #[allow(dead_code)] // Private OPT-1857 producer adapter has no ordinary-family caller yet.
    pub(crate) fn for_live_chunk(
        grant: &LiveChunkEffectGrant,
        effect: EffectRequest,
    ) -> Result<Self, FileOperationValidationError> {
        if !matches!(effect, EffectRequest::WriteStagingChunk { .. }) {
            return Err(FileOperationValidationError::new(
                "journal chunk grant requires a staging chunk effect",
            ));
        }
        let target = effect.target_binding();
        let command = Self {
            operation_id: grant.operation_id(),
            participant_id: grant.participant_id(),
            effect_id: grant.effect_id(),
            source_fences: grant.source_fences().to_vec(),
            target_registration: grant.target(),
            expected: target.expected,
            effect,
            authorization: EffectAuthorization::JournalGrant(JournalGrantAuthorization {
                _sealed: (),
            }),
        };
        command.validate_shape()?;
        if !grant.matches_command(&command) {
            return Err(FileOperationValidationError::new(
                "staging chunk command does not match its journal grant",
            ));
        }
        Ok(command)
    }

    #[allow(dead_code)] // Private OPT-1857 producer adapter has no ordinary-family caller yet.
    pub(crate) fn retry_for_live_chunk(&self) -> Option<Self> {
        matches!(self.authorization, EffectAuthorization::JournalGrant(_)).then(|| Self {
            operation_id: self.operation_id,
            participant_id: self.participant_id,
            effect_id: self.effect_id,
            source_fences: self.source_fences.clone(),
            target_registration: self.target_registration,
            expected: self.expected,
            effect: self.effect.clone(),
            authorization: EffectAuthorization::JournalGrant(JournalGrantAuthorization {
                _sealed: (),
            }),
        })
    }

    /// Return the coordinator operation identity.
    pub const fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    /// Return the participant identity.
    pub const fn participant_id(&self) -> ParticipantId {
        self.participant_id
    }

    /// Return the effect identity.
    pub const fn effect_id(&self) -> EffectId {
        self.effect_id
    }

    /// Return the complete effect payload.
    pub fn effect(&self) -> &EffectRequest {
        &self.effect
    }

    pub(crate) fn source_fences(&self) -> &[SourceFence] {
        &self.source_fences
    }

    pub(crate) fn target_registration(&self) -> RootRegistrationRef {
        self.target_registration
    }

    /// Validate the authority-free shape and encode a collision-safe request fingerprint.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, FileOperationValidationError> {
        self.validate_shape()?;
        let view = AuthorityFreeCommandView {
            operation_id: self.operation_id,
            participant_id: self.participant_id,
            effect_id: self.effect_id,
            source_fences: &self.source_fences,
            target_registration: self.target_registration,
            expected: self.expected,
            effect: &self.effect,
        };
        let bytes = serde_json::to_vec(&view)
            .map_err(|error| FileOperationValidationError::new(error.to_string()))?;
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(FileOperationValidationError::new(
                "encoded command exceeds the owner record bound",
            ));
        }
        Ok(bytes)
    }

    fn normalize_storage(&mut self) {
        self.source_fences = self.source_fences.iter().cloned().collect();
        match &mut self.authorization {
            EffectAuthorization::Mutating { live_intent, .. } => {
                live_intent.source_fences = live_intent.source_fences.iter().cloned().collect();
            }
            EffectAuthorization::ObservationOnly { source_fences, .. } => {
                *source_fences = source_fences.iter().cloned().collect();
            }
            EffectAuthorization::JournalGrant(_) => {}
        }
    }

    /// Return the owned authorization backing excluded from the authority-free fingerprint.
    fn authorization_backing_bytes(&self) -> Option<usize> {
        let source_fences = match &self.authorization {
            EffectAuthorization::Mutating { live_intent, .. } => &live_intent.source_fences,
            EffectAuthorization::ObservationOnly { source_fences, .. } => source_fences,
            EffectAuthorization::JournalGrant(_) => return Some(0),
        };
        let mut bytes = source_fences
            .capacity()
            .checked_mul(std::mem::size_of::<SourceFence>())?;
        for fence in source_fences {
            bytes = bytes.checked_add(fence.dynamic_backing_bytes()?)?;
        }
        Some(bytes)
    }

    fn validate_shape(&self) -> Result<(), FileOperationValidationError> {
        if self.source_fences.len() > FILE_OPERATION_OWNER_MAX_SOURCE_FENCES {
            return Err(FileOperationValidationError::new(
                "command exceeds the source-fence bound",
            ));
        }
        for fence in &self.source_fences {
            fence.validate()?;
        }
        self.effect.validate_shape(&self.source_fences)?;
        let role_refs = self.effect.role_bindings();
        if role_refs.len() > FILE_OPERATION_OWNER_MAX_ROOTS {
            return Err(FileOperationValidationError::new(
                "effect exceeds the registered-root bound",
            ));
        }
        let target = self.effect.target_binding();
        if target.registration != self.target_registration {
            return Err(FileOperationValidationError::new(
                "target registration must identify the effect destination role",
            ));
        }
        if target.expected != self.expected {
            return Err(FileOperationValidationError::new(
                "command expected predicate must match the destination role",
            ));
        }
        for (_, binding) in &role_refs {
            if let AuthorityFence::Source(fence) = binding.authority() {
                if !self.source_fences.contains(fence) {
                    return Err(FileOperationValidationError::new(
                        "source root authority must be echoed in command source fences",
                    ));
                }
            }
        }
        match &self.authorization {
            EffectAuthorization::Mutating {
                live_intent,
                live_capacity,
            } => {
                if live_intent.nonce == 0 || live_capacity.nonce == 0 {
                    return Err(FileOperationValidationError::new(
                        "live authorization nonce must be positive",
                    ));
                }
                if live_intent.source_fences.len() > FILE_OPERATION_OWNER_MAX_SOURCE_FENCES {
                    return Err(FileOperationValidationError::new(
                        "live intent exceeds the source-fence bound",
                    ));
                }
                for fence in &live_intent.source_fences {
                    fence.validate()?;
                }
            }
            EffectAuthorization::ObservationOnly {
                live_root_registration,
                source_fences,
            } => {
                if !matches!(self.effect, EffectRequest::InspectOwnedObject { .. }) {
                    return Err(FileOperationValidationError::new(
                        "observation-only authorization is valid only for inspection",
                    ));
                }
                if source_fences.len() > FILE_OPERATION_OWNER_MAX_SOURCE_FENCES {
                    return Err(FileOperationValidationError::new(
                        "observation authorization exceeds the source-fence bound",
                    ));
                }
                if *live_root_registration != self.target_registration {
                    return Err(FileOperationValidationError::new(
                        "observation authorization root does not match the target",
                    ));
                }
                for fence in source_fences {
                    fence.validate()?;
                }
            }
            EffectAuthorization::JournalGrant(_) => {
                if !matches!(self.effect, EffectRequest::WriteStagingChunk { .. }) {
                    return Err(FileOperationValidationError::new(
                        "journal grant is valid only for staging chunk writes",
                    ));
                }
            }
        }
        if !matches!(self.effect, EffectRequest::InspectOwnedObject { .. })
            && matches!(
                self.authorization,
                EffectAuthorization::ObservationOnly { .. }
            )
        {
            return Err(FileOperationValidationError::new(
                "mutating effect requires live intent and capacity authorization",
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    fn for_test(
        operation_id: OperationId,
        participant_id: ParticipantId,
        effect_id: EffectId,
        source_fences: Vec<SourceFence>,
        target_registration: RootRegistrationRef,
        expected: ExpectedObject,
        effect: EffectRequest,
        authorization: EffectAuthorization,
    ) -> Self {
        Self {
            operation_id,
            participant_id,
            effect_id,
            source_fences,
            target_registration,
            expected,
            effect,
            authorization,
        }
    }

    #[cfg(test)]
    fn clone_for_test(&self) -> Self {
        let authorization = match &self.authorization {
            EffectAuthorization::Mutating {
                live_intent,
                live_capacity,
            } => EffectAuthorization::Mutating {
                live_intent: LiveIntentAuthorization {
                    owner: live_intent.owner,
                    nonce: live_intent.nonce,
                    source_fences: live_intent.source_fences.clone(),
                },
                live_capacity: LiveCapacityAuthorization {
                    owner: live_capacity.owner,
                    nonce: live_capacity.nonce,
                    available_bytes: live_capacity.available_bytes,
                    protected_floor: live_capacity.protected_floor,
                },
            },
            EffectAuthorization::ObservationOnly {
                live_root_registration,
                source_fences,
            } => EffectAuthorization::ObservationOnly {
                live_root_registration: *live_root_registration,
                source_fences: source_fences.clone(),
            },
            EffectAuthorization::JournalGrant(_) => {
                EffectAuthorization::JournalGrant(JournalGrantAuthorization { _sealed: () })
            }
        };
        Self {
            operation_id: self.operation_id,
            participant_id: self.participant_id,
            effect_id: self.effect_id,
            source_fences: self.source_fences.clone(),
            target_registration: self.target_registration,
            expected: self.expected,
            effect: self.effect.clone(),
            authorization,
        }
    }
}

impl EffectRequest {
    fn target_binding(&self) -> &RootRoleBinding {
        match self {
            Self::CreateStaging { destination, .. }
            | Self::CreateDirectoryNoReplace { destination, .. } => &destination.binding,
            Self::CopyIntoStaging { staging, .. }
            | Self::WriteStagingChunk { staging, .. }
            | Self::FlushStaging { staging, .. }
            | Self::ValidateStaging { staging, .. } => &staging.binding,
            Self::PublishNoReplace { target, .. } | Self::ReplaceExpectedTarget { target, .. } => {
                &target.binding
            }
            Self::MoveNoReplace { destination, .. } => &destination.binding,
            Self::InspectOwnedObject { root, .. }
            | Self::RemoveOwnedObject { root, .. }
            | Self::RestoreOwnedObject { root, .. } => &root.binding,
        }
    }

    fn role_bindings(&self) -> Vec<(RootRole, &RootRoleBinding)> {
        match self {
            Self::CreateStaging { destination, .. } => {
                vec![(RootRole::Destination, &destination.binding)]
            }
            Self::CopyIntoStaging {
                source, staging, ..
            } => {
                vec![
                    (RootRole::Source, &source.binding),
                    (RootRole::Staging, &staging.binding),
                ]
            }
            Self::WriteStagingChunk { staging, .. }
            | Self::FlushStaging { staging, .. }
            | Self::ValidateStaging { staging, .. } => {
                vec![(RootRole::Staging, &staging.binding)]
            }
            Self::PublishNoReplace {
                staging, target, ..
            }
            | Self::ReplaceExpectedTarget {
                staging, target, ..
            } => {
                vec![
                    (RootRole::Staging, &staging.binding),
                    (RootRole::Destination, &target.binding),
                ]
            }
            Self::MoveNoReplace {
                source,
                destination,
                ..
            } => vec![
                (RootRole::Source, &source.binding),
                (RootRole::Destination, &destination.binding),
            ],
            Self::CreateDirectoryNoReplace { destination, .. } => {
                vec![(RootRole::Destination, &destination.binding)]
            }
            Self::InspectOwnedObject { root, .. }
            | Self::RemoveOwnedObject { root, .. }
            | Self::RestoreOwnedObject { root, .. } => {
                vec![(RootRole::Owned, &root.binding)]
            }
        }
    }

    fn validate_shape(
        &self,
        command_fences: &[SourceFence],
    ) -> Result<(), FileOperationValidationError> {
        let bindings = self.role_bindings();
        for (_, binding) in &bindings {
            binding.locator.validate()?;
            if let AuthorityFence::Profile(profile) = &binding.authority {
                if profile.lease_epoch == 0 {
                    return Err(FileOperationValidationError::new(
                        "profile authority lease epoch must be positive",
                    ));
                }
            }
        }
        let mut source_fences = command_fences.to_vec();
        for (_, binding) in &bindings {
            if let AuthorityFence::Source(fence) = &binding.authority {
                if !source_fences.contains(fence) {
                    source_fences.push(fence.clone());
                }
            }
        }
        if source_fences.len() > FILE_OPERATION_OWNER_MAX_SOURCE_FENCES {
            return Err(FileOperationValidationError::new(
                "effect exceeds the source-fence bound",
            ));
        }
        match self {
            Self::CreateStaging {
                destination,
                expected_absent,
                bytes,
            } => {
                require_absent(&destination.binding, *expected_absent)?;
                if bytes.get() == 0 {
                    return Err(FileOperationValidationError::new(
                        "staging size must be positive",
                    ));
                }
            }
            Self::CopyIntoStaging {
                source,
                staging,
                source_range,
                bytes,
            } => {
                require_source_object(&source.binding)?;
                require_present(&staging.binding)?;
                source_range
                    .offset
                    .get()
                    .checked_add(source_range.length.get())
                    .ok_or_else(|| FileOperationValidationError::new("copy range overflows"))?;
                if bytes.get() < source_range.length.get() {
                    return Err(FileOperationValidationError::new(
                        "copy allocation must cover the complete source range",
                    ));
                }
                if bytes.get() == 0 {
                    return Err(FileOperationValidationError::new(
                        "copy size must be positive",
                    ));
                }
            }
            Self::WriteStagingChunk {
                staging,
                offset,
                length,
                payload,
                ..
            } => {
                require_present(&staging.binding)?;
                offset
                    .get()
                    .checked_add(length.get())
                    .ok_or_else(|| FileOperationValidationError::new("write offset overflows"))?;
                if payload.as_slice().is_empty() || payload.as_slice().len() as u64 > length.get() {
                    return Err(FileOperationValidationError::new(
                        "write payload must be non-empty and fit its declared length",
                    ));
                }
            }
            Self::FlushStaging { staging, .. } | Self::ValidateStaging { staging, .. } => {
                require_present(&staging.binding)?;
                let object = match self {
                    Self::FlushStaging { object, .. } => *object,
                    Self::ValidateStaging { object, .. } => *object,
                    _ => unreachable!("matched staging validation effect"),
                };
                require_binding_object(&staging.binding, object, "staging")?;
            }
            Self::PublishNoReplace {
                staging,
                target,
                staged,
                expected_absent,
                ..
            } => {
                require_present(&staging.binding)?;
                require_absent(&target.binding, *expected_absent)?;
                require_binding_object(&staging.binding, staged.object, "staging")?;
            }
            Self::MoveNoReplace {
                source,
                destination,
                source_object,
                expected_absent,
                ..
            } => {
                require_source_object(&source.binding)?;
                require_absent(&destination.binding, *expected_absent)?;
                if *source_object != expected_object(&source.binding)? {
                    return Err(FileOperationValidationError::new(
                        "move source identity must match the source role predicate",
                    ));
                }
            }
            Self::CreateDirectoryNoReplace {
                destination,
                expected_absent,
            } => require_absent(&destination.binding, *expected_absent)?,
            Self::ReplaceExpectedTarget {
                staging,
                target,
                staged,
                expected_target,
            } => {
                require_present(&staging.binding)?;
                require_binding_object(&staging.binding, staged.object, "staging")?;
                require_binding_object(&target.binding, *expected_target, "replace target")?;
            }
            Self::InspectOwnedObject { root, expected } => {
                if root.binding.expected != *expected {
                    return Err(FileOperationValidationError::new(
                        "inspection predicate must match its root binding",
                    ));
                }
            }
            Self::RemoveOwnedObject { root, expected, .. }
            | Self::RestoreOwnedObject { root, expected, .. } => {
                if root.binding.expected != ExpectedObject::Exact(*expected) {
                    return Err(FileOperationValidationError::new(
                        "cleanup predicate must match its root binding",
                    ));
                }
            }
        }
        Ok(())
    }

    fn required_bytes(&self) -> u64 {
        match self {
            Self::CreateStaging { bytes, .. } | Self::CopyIntoStaging { bytes, .. } => bytes.get(),
            Self::WriteStagingChunk { length, .. } => length.get(),
            Self::FlushStaging { .. }
            | Self::ValidateStaging { .. }
            | Self::PublishNoReplace { .. }
            | Self::MoveNoReplace { .. }
            | Self::CreateDirectoryNoReplace { .. }
            | Self::ReplaceExpectedTarget { .. }
            | Self::InspectOwnedObject { .. }
            | Self::RemoveOwnedObject { .. }
            | Self::RestoreOwnedObject { .. } => 0,
        }
    }
}

/// Typed outcome of an accepted effect request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EffectOutcome {
    /// The owner applied an effect and returned observations.  Current production has no
    /// backend, so this variant is reserved for a future qualified adapter and test fixtures.
    Applied {
        /// Effect observations.
        evidence: EffectEvidence,
    },
    /// A live fence was stale before any effect could begin.
    SupersededBeforeEffect {
        /// Bounded reason.
        reason: String,
    },
    /// An exact expected identity or absence predicate failed.
    Collision {
        /// Bounded observed collision detail.
        reason: String,
    },
    /// No qualified producer/backend authority exists; no effect was attempted.
    Unsupported {
        /// Bounded unsupported reason.
        reason: String,
    },
    /// The request may be retried before any effect.
    RetryableBeforeEffect {
        /// Bounded retry reason.
        reason: String,
    },
    /// An effect or acknowledgement may be uncertain; evidence remains retained.
    RetainedUncertain {
        /// Bounded audit reason.
        reason: String,
    },
    /// The owner stopped at a confirmed safe barrier.
    StoppedAtSafeBoundary {
        /// Bounded checkpoint observation.
        checkpoint: String,
    },
    /// A changed request attempted to reuse an existing effect identity.
    Conflict {
        /// Fingerprint of the retained original request.
        original_fingerprint: RequestFingerprint,
    },
}

/// Authority-free observations returned by a successful effect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectEvidence {
    observed_identity: Option<OwnedObjectIdentity>,
    bytes: Option<u64>,
}

impl EffectEvidence {
    /// Return the observed object identity, if any.
    pub const fn observed_identity(&self) -> Option<OwnedObjectIdentity> {
        self.observed_identity
    }

    /// Return the observed byte count, if any.
    pub const fn bytes(&self) -> Option<u64> {
        self.bytes
    }
}

/// Collision-safe fingerprint of a complete bounded request.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RequestFingerprint([u8; 32]);

impl RequestFingerprint {
    /// Return the digest bytes for diagnostics and exact comparison.
    pub const fn as_bytes(self) -> [u8; 32] {
        self.0
    }
}

/// Authority-free observation of one bound root role in a result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RootBindingObservation {
    role: RootRole,
    registration: RootRegistrationRef,
    authority: AuthorityFence,
    locator: BoundedRelativeLocator,
    expected: ExpectedObject,
}

impl RootBindingObservation {
    /// Return the role associated with this authority-free observation.
    pub const fn role(&self) -> RootRole {
        self.role
    }

    /// Return the observed registration reference.
    pub const fn registration(&self) -> RootRegistrationRef {
        self.registration
    }

    /// Return the observed authority fence.
    pub fn authority(&self) -> &AuthorityFence {
        &self.authority
    }

    /// Return the observed locator.
    pub fn locator(&self) -> &BoundedRelativeLocator {
        &self.locator
    }

    /// Return the observed expected predicate.
    pub const fn expected(&self) -> ExpectedObject {
        self.expected
    }
}

/// Result receipt for one accepted file-owner command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileOperationResult {
    operation_id: OperationId,
    participant_id: ParticipantId,
    effect_id: EffectId,
    target_registration: RootRegistrationRef,
    source_fences: Vec<SourceFence>,
    root_bindings: Vec<RootBindingObservation>,
    expected: ExpectedObject,
    fingerprint: RequestFingerprint,
    outcome: EffectOutcome,
}

impl FileOperationResult {
    /// Return the operation identity.
    pub const fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    /// Return the participant identity.
    pub const fn participant_id(&self) -> ParticipantId {
        self.participant_id
    }

    /// Return the effect identity.
    pub const fn effect_id(&self) -> EffectId {
        self.effect_id
    }

    /// Return the primary destination/owned registration observation.
    pub const fn target_registration(&self) -> RootRegistrationRef {
        self.target_registration
    }

    /// Return the complete request fingerprint.
    pub const fn fingerprint(&self) -> RequestFingerprint {
        self.fingerprint
    }

    /// Return the authority-free source-fence observations.
    pub fn source_fences(&self) -> &[SourceFence] {
        &self.source_fences
    }

    /// Return all bound root-role observations.
    pub fn root_bindings(&self) -> &[RootBindingObservation] {
        &self.root_bindings
    }

    /// Return the expected object predicate.
    pub const fn expected(&self) -> ExpectedObject {
        self.expected
    }

    /// Return the typed effect outcome.
    pub fn outcome(&self) -> &EffectOutcome {
        &self.outcome
    }
}

/// Lifecycle visible at the file-owner boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileOperationOwnerStatus {
    /// The owner accepts new commands.
    Running,
    /// New effects are closed while accepted commands and acknowledgements drain.
    Draining,
    /// The owner thread has stopped and accepts no work.
    Stopped,
    /// The owner failed and remains unavailable for the rest of its lifetime.
    Unavailable {
        /// Bounded failure reason.
        reason: String,
    },
}

/// One effect identity retained when orderly shutdown cannot claim custody.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PendingEffectIdentity {
    operation_id: OperationId,
    participant_id: ParticipantId,
    effect_id: EffectId,
}

impl PendingEffectIdentity {
    /// Return the operation identity.
    pub const fn operation_id(self) -> OperationId {
        self.operation_id
    }

    /// Return the participant identity.
    pub const fn participant_id(self) -> ParticipantId {
        self.participant_id
    }

    /// Return the effect identity.
    pub const fn effect_id(self) -> EffectId {
        self.effect_id
    }
}

/// Outcome of explicit owner shutdown.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileOperationOwnerShutdown {
    /// All accepted work had a confirmed safe boundary and no receipt remained without custody.
    DrainedAck,
    /// Shutdown completed with bounded evidence still retained for explicit reconciliation.
    RetirementDeferred {
        /// Exact pending effect identities.
        pending: Vec<PendingEffectIdentity>,
        /// Bounded reason.
        reason: String,
    },
    /// Shutdown or worker failure made acknowledgement uncertain.
    RetirementUncertain {
        /// Exact pending effect identities known at the boundary.
        pending: Vec<PendingEffectIdentity>,
        /// Bounded reason.
        reason: String,
    },
}

/// Bounded result of requesting cancellation for one known effect identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileOperationCancelOutcome {
    /// The owner retained the effect and recorded the bounded cancellation observation.
    KnownPending,
    /// The effect crossed its execution boundary; cancellation cannot claim completion or undo it.
    AlreadyStarted,
    /// The effect was already completed and remains retained for exact duplicate replay.
    AlreadyCompleted,
    /// No accepted effect with this identity is retained; no cancellation tombstone was created.
    Unknown,
}

/// Failure to spawn the owner worker.
#[derive(Debug, Error)]
pub enum FileOperationOwnerStartError {
    /// The operating system rejected worker creation.
    #[error("failed to start file-operation owner thread: {0}")]
    ThreadSpawn(#[source] std::io::Error),
    /// The worker exited before publishing its running state.
    #[error("file-operation owner stopped during startup")]
    StartupClosed,
}

/// Nonblocking failure to queue a file-owner command.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum FileOperationOwnerQueueError {
    /// The command queue is full.
    #[error("file-operation owner command queue is full")]
    Full,
    /// All bounded result permits are in use.
    #[error("file-operation owner result capacity is full")]
    ResultsFull,
    /// The command input-byte budget is exhausted.
    #[error("file-operation owner input budget is full: requested {requested} bytes")]
    InputBytesFull {
        /// Requested canonical input bytes.
        requested: usize,
    },
    /// The command exceeds the bounded record size.
    #[error("file-operation owner command is too large: {actual} bytes")]
    RecordTooLarge {
        /// Actual encoded command bytes.
        actual: usize,
    },
    /// The command shape failed before queue admission.
    #[error("file-operation owner command is invalid: {0}")]
    InvalidInput(#[from] FileOperationValidationError),
    /// The owner is unavailable after a panic or unrecoverable failure.
    #[error("file-operation owner is unavailable: {0}")]
    Unavailable(String),
    /// The owner is draining and has closed new effects.
    #[error("file-operation owner is draining")]
    Draining,
    /// The owner has stopped.
    #[error("file-operation owner is stopped")]
    Stopped,
}

/// Failure while waiting for an accepted result.
#[derive(Debug, Error)]
pub enum FileOperationRequestError {
    /// No result arrived before the requested timeout.
    #[error("file-operation owner result timed out")]
    Timeout,
    /// The owner stopped without delivering the result.
    #[error("file-operation owner result channel closed")]
    Disconnected,
    /// The result has already been consumed.
    #[error("file-operation owner result was already consumed")]
    AlreadyConsumed,
    /// Acknowledgement could not be queued.
    #[error("file-operation owner acknowledgement could not be queued: {0}")]
    Acknowledgement(#[from] FileOperationOwnerQueueError),
}

/// A request handle retaining one bounded result permit.
pub struct FileOperationRequest {
    receiver: Receiver<Arc<FileOperationResult>>,
    shared: Arc<OwnerShared>,
    key: EffectKey,
    fingerprint: RequestFingerprint,
    received: bool,
    acknowledged: bool,
    _result_permit: Option<ResultPermit>,
}

impl FileOperationRequest {
    /// Wait for the accepted result.
    pub fn recv(&mut self) -> Result<FileOperationResult, FileOperationRequestError> {
        if self.received {
            return Err(FileOperationRequestError::AlreadyConsumed);
        }
        match self.receiver.recv() {
            Ok(result) => {
                self.received = true;
                Ok((*result).clone())
            }
            Err(_) => Err(FileOperationRequestError::Disconnected),
        }
    }

    /// Wait for the accepted result for at most `timeout`.
    pub fn recv_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<FileOperationResult, FileOperationRequestError> {
        if self.received {
            return Err(FileOperationRequestError::AlreadyConsumed);
        }
        match self.receiver.recv_timeout(timeout) {
            Ok(result) => {
                self.received = true;
                Ok((*result).clone())
            }
            Err(RecvTimeoutError::Timeout) => Err(FileOperationRequestError::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(FileOperationRequestError::Disconnected),
        }
    }

    /// Acknowledge receipt so the owner may release retained canonical input and receipt state.
    pub fn acknowledge(&mut self) -> Result<(), FileOperationRequestError> {
        if !self.received {
            return Err(FileOperationRequestError::Acknowledgement(
                FileOperationOwnerQueueError::InvalidInput(FileOperationValidationError::new(
                    "a result must be consumed before acknowledgement",
                )),
            ));
        }
        if self.acknowledged {
            return Ok(());
        }
        self.shared.try_acknowledge(self.key, self.fingerprint)?;
        self.acknowledged = true;
        Ok(())
    }

    /// Recover the original sealed permit after a confirmed no-effect result. A retry request
    /// may do this after the primary reply was dropped; the grant can be taken only once.
    #[allow(dead_code)] // No ordinary-family rollback producer exists yet.
    pub(crate) fn take_unused_live_chunk_grant(
        &self,
    ) -> Result<Option<LiveChunkEffectGrant>, FileOperationOwnerQueueError> {
        if !self.received {
            return Err(FileOperationOwnerQueueError::InvalidInput(
                FileOperationValidationError::new(
                    "a result must be consumed before grant recovery",
                ),
            ));
        }
        self.shared
            .try_take_unused_live_chunk_grant(self.key, self.fingerprint)
    }
}

impl Drop for FileOperationRequest {
    fn drop(&mut self) {
        if self.received && !self.acknowledged {
            if self
                .shared
                .try_acknowledge(self.key, self.fingerprint)
                .is_ok()
            {
                self.acknowledged = true;
            }
        }
    }
}

/// A bounded owner of file-operation command execution and receipt state.
pub struct FileOperationOwner {
    shared: Arc<OwnerShared>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl FileOperationOwner {
    /// Start the isolated owner worker.
    pub fn start() -> Result<Self, FileOperationOwnerStartError> {
        let owner = OwnerIdentity(Uuid::new_v4());
        let (command_tx, command_rx) = mpsc::sync_channel(FILE_OPERATION_OWNER_COMMAND_CAPACITY);
        let shared = Arc::new(OwnerShared::new(owner, command_tx));
        let worker_shared = Arc::clone(&shared);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let join = thread::Builder::new()
            .name(String::from("yield-file-operation-owner"))
            .spawn(move || worker_main(worker_shared, command_rx, ready_tx))
            .map_err(FileOperationOwnerStartError::ThreadSpawn)?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                shared,
                join: Mutex::new(Some(join)),
            }),
            Ok(Err(_reason)) => {
                let _ = join.join();
                Err(FileOperationOwnerStartError::StartupClosed)
            }
            Err(_) => {
                let _ = join.join();
                Err(FileOperationOwnerStartError::StartupClosed)
            }
        }
    }

    /// Return the owner identity as an observation.
    pub fn owner_identity(&self) -> OwnerIdentity {
        self.shared.owner
    }

    /// Return the current owner lifecycle.
    pub fn status(&self) -> FileOperationOwnerStatus {
        self.shared.status()
    }

    /// Queue one validated bounded effect command.
    pub fn submit(
        &self,
        command: FileOperationCommand,
    ) -> Result<FileOperationRequest, FileOperationOwnerQueueError> {
        self.submit_inner(command, None).map_err(|(error, _)| error)
    }

    #[allow(dead_code)] // No ordinary-family producer submits live chunks yet.
    pub(crate) fn submit_live_chunk(
        &self,
        command: FileOperationCommand,
        grant: LiveChunkEffectGrant,
    ) -> Result<FileOperationRequest, (FileOperationOwnerQueueError, LiveChunkEffectGrant)> {
        self.submit_inner(command, Some(grant))
            .map_err(|(error, grant)| (error, grant.expect("primary grant returned on rejection")))
    }

    #[allow(dead_code)] // No ordinary-family producer retries live chunks yet.
    pub(crate) fn retry_live_chunk(
        &self,
        command: FileOperationCommand,
    ) -> Result<FileOperationRequest, FileOperationOwnerQueueError> {
        self.submit_inner(command, None).map_err(|(error, _)| error)
    }

    fn submit_inner(
        &self,
        mut command: FileOperationCommand,
        mut grant: Option<LiveChunkEffectGrant>,
    ) -> Result<FileOperationRequest, (FileOperationOwnerQueueError, Option<LiveChunkEffectGrant>)>
    {
        macro_rules! reject {
            ($error:expr) => {
                return Err(($error, grant.take()))
            };
        }
        command.normalize_storage();
        let canonical = match command.canonical_bytes() {
            Ok(canonical) => canonical,
            Err(error) => reject!(FileOperationOwnerQueueError::InvalidInput(error)),
        };
        if canonical.len() > MAX_RECORD_BYTES {
            reject!(FileOperationOwnerQueueError::RecordTooLarge {
                actual: canonical.len(),
            });
        }
        let fingerprint = RequestFingerprint(*blake3::hash(&canonical).as_bytes());
        let key = EffectKey::from_command(&command);
        let _gate = self
            .shared
            .admission_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Err(error) = self.shared.ensure_accepting() {
            reject!(error);
        }
        let primary = {
            let admissions = self
                .shared
                .admissions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !admissions.contains_key(&key)
                && admissions.len() >= FILE_OPERATION_OWNER_MAX_RECEIPTS
            {
                reject!(FileOperationOwnerQueueError::ResultsFull);
            }
            !admissions.contains_key(&key)
        };
        match (&command.authorization, grant.as_ref(), primary) {
            (EffectAuthorization::JournalGrant(_), Some(live), true)
                if live.matches_command(&command) => {}
            (EffectAuthorization::JournalGrant(_), None, false) => {}
            (EffectAuthorization::JournalGrant(_), _, _) => {
                reject!(FileOperationOwnerQueueError::InvalidInput(
                    FileOperationValidationError::new(
                        "journal command requires one exact grant for its primary admission",
                    ),
                ));
            }
            (_, Some(_), _) => {
                reject!(FileOperationOwnerQueueError::InvalidInput(
                    FileOperationValidationError::new(
                        "journal grant cannot accompany another authorization mode",
                    ),
                ));
            }
            (_, None, _) => {}
        }
        let authorization_backing = command.authorization_backing_bytes();
        let retained_bytes = match canonical
            .len()
            .checked_mul(2)
            .and_then(|bytes| authorization_backing.and_then(|extra| bytes.checked_add(extra)))
            .and_then(|bytes| {
                grant.as_ref().map_or(Some(bytes), |live| {
                    live.queued_backing_bytes()
                        .and_then(|extra| bytes.checked_add(extra))
                })
            })
            .ok_or_else(|| FileOperationOwnerQueueError::RecordTooLarge { actual: usize::MAX })
        {
            Ok(bytes) => bytes,
            Err(error) => reject!(error),
        };
        let input = match self.shared.reserve_input(retained_bytes) {
            Ok(input) => input,
            Err(error) => reject!(error),
        };
        let result_permit = match self.shared.reserve_result() {
            Ok(permit) => permit,
            Err(error) => {
                drop(input);
                reject!(error);
            }
        };
        let canonical = Arc::from(canonical.into_boxed_slice());
        let command_input = if primary {
            self.shared
                .admissions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(
                    key,
                    AdmissionRecord {
                        canonical: Arc::clone(&canonical),
                        fingerprint,
                        input,
                        state: AdmissionState::Queued,
                        cancel_requested: false,
                    },
                );
            None
        } else {
            Some(input)
        };
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let envelope = OwnerCommand::Execute {
            command,
            grant: grant.take(),
            canonical,
            fingerprint,
            key,
            input: command_input,
            primary,
            result: result_tx,
        };
        let sender = self
            .shared
            .commands
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .cloned()
            .ok_or_else(|| FileOperationOwnerQueueError::Stopped);
        let sender = match sender {
            Ok(sender) => sender,
            Err(error) => {
                if primary {
                    self.shared
                        .admissions
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(&key);
                }
                drop(result_permit);
                let grant = match envelope {
                    OwnerCommand::Execute { grant, .. } => grant,
                    _ => unreachable!("execute envelope"),
                };
                return Err((error, grant));
            }
        };
        match sender.try_send(envelope) {
            Ok(()) => Ok(FileOperationRequest {
                receiver: result_rx,
                shared: Arc::clone(&self.shared),
                key,
                fingerprint,
                received: false,
                acknowledged: false,
                _result_permit: Some(result_permit),
            }),
            Err(TrySendError::Full(envelope)) => {
                let grant = match envelope {
                    OwnerCommand::Execute { grant, .. } => grant,
                    _ => unreachable!("execute envelope"),
                };
                if primary {
                    self.shared
                        .admissions
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(&key);
                }
                drop(result_permit);
                Err((FileOperationOwnerQueueError::Full, grant))
            }
            Err(TrySendError::Disconnected(envelope)) => {
                let grant = match envelope {
                    OwnerCommand::Execute { grant, .. } => grant,
                    _ => unreachable!("execute envelope"),
                };
                if primary {
                    self.shared
                        .admissions
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(&key);
                }
                drop(result_permit);
                self.shared
                    .mark_unavailable("owner command channel disconnected");
                Err((
                    FileOperationOwnerQueueError::Unavailable(String::from(
                        "owner command channel disconnected",
                    )),
                    grant,
                ))
            }
        }
    }

    /// Close the front door and let already accepted commands drain.
    pub fn begin_drain(&self) -> Result<(), FileOperationOwnerQueueError> {
        self.shared.begin_drain()
    }

    /// Explicitly shut down the owner and return the custody-aware result.
    pub fn shutdown(&self) -> FileOperationOwnerShutdown {
        self.shared.explicit_close.store(true, Ordering::Release);
        self.shared.close_commands();
        let join = self
            .join
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(join) = join {
            let _ = join.join();
        }
        self.shared
            .shutdown_outcome
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .unwrap_or_else(|| FileOperationOwnerShutdown::RetirementUncertain {
                pending: self.shared.pending_effects(),
                reason: String::from("owner stopped without a custody result"),
            })
    }

    /// Return exact effect identities retained as observations by this owner.
    pub fn pending_effects(&self) -> Vec<PendingEffectIdentity> {
        self.shared.pending_effects()
    }

    /// Request cancellation for one already-known effect identity without waiting on the worker.
    ///
    /// Unknown identities are rejected without creating a tombstone or changing owner state.
    /// A queued identity is marked before its execution boundary; an executing identity returns
    /// [`FileOperationCancelOutcome::AlreadyStarted`] and a terminal identity returns
    /// [`FileOperationCancelOutcome::AlreadyCompleted`].
    pub fn cancel(
        &self,
        identity: PendingEffectIdentity,
    ) -> Result<FileOperationCancelOutcome, FileOperationOwnerQueueError> {
        self.shared.try_cancel(EffectKey {
            operation_id: identity.operation_id,
            participant_id: identity.participant_id,
            effect_id: identity.effect_id,
        })
    }

    #[cfg(test)]
    fn register_root_for_test(
        &self,
        authority: AuthorityFence,
        label: &str,
    ) -> Result<RootRegistrationRef, String> {
        self.register_root_with_generation_for_test(authority, label, 1)
    }

    #[cfg(test)]
    fn register_root_with_generation_for_test(
        &self,
        authority: AuthorityFence,
        label: &str,
        generation: u64,
    ) -> Result<RootRegistrationRef, String> {
        let registration = RootRegistrationRef {
            owner: self.shared.owner,
            registration_id: Uuid::new_v4(),
            generation,
        };
        let label = bounded_diagnostic(label.to_owned());
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        self.shared
            .try_control(OwnerCommand::RegisterRoot {
                registration,
                authority,
                label,
                input: None,
                result: result_tx,
            })
            .map_err(|error| error.to_string())?;
        result_rx
            .recv_timeout(Duration::from_secs(1))
            .map_err(|_| String::from("root registration timed out"))??;
        Ok(registration)
    }

    #[cfg(test)]
    pub(crate) fn register_live_chunk_root_for_test(&self) -> Result<RootRegistrationRef, String> {
        self.register_root_for_test(
            AuthorityFence::Profile(ProfileAuthorityFence {
                owner: self.shared.owner,
                generation: 1,
                lease_epoch: 1,
            }),
            "live chunk staging root",
        )
    }

    #[cfg(test)]
    pub(crate) fn live_chunk_effect_for_test(
        &self,
        grant: &LiveChunkEffectGrant,
        payload: Vec<u8>,
    ) -> EffectRequest {
        let staging = StagingRoot {
            binding: RootRoleBinding {
                registration: grant.target(),
                authority: AuthorityFence::Profile(ProfileAuthorityFence {
                    owner: self.shared.owner,
                    generation: 1,
                    lease_epoch: 1,
                }),
                locator: BoundedRelativeLocator::new("staging.bin").expect("staging locator"),
                expected: ExpectedObject::Exact(OwnedObjectIdentity::new()),
            },
        };
        EffectRequest::WriteStagingChunk {
            staging,
            chunk_id: ChunkId::from_uuid(grant.chunk_id()),
            issue_id: IssueId::from_uuid(grant.issue_id()),
            output: grant.output(),
            offset: BoundedOffset::new(grant.offset()),
            length: PositiveBoundedBytes::new(grant.logical_bytes()).expect("bounded grant"),
            payload: BoundedBytes::new(payload).expect("bounded payload"),
        }
    }

    #[cfg(test)]
    pub(crate) fn retire_root_for_test(
        &self,
        registration: RootRegistrationRef,
    ) -> Result<(), String> {
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        self.shared
            .try_control(OwnerCommand::RetireRoot {
                registration,
                input: None,
                result: result_tx,
            })
            .map_err(|error| error.to_string())?;
        result_rx
            .recv_timeout(Duration::from_secs(1))
            .map_err(|_| String::from("root retirement timed out"))??;
        Ok(())
    }

    #[cfg(test)]
    fn test_mutating_authorization(
        &self,
        source_fences: Vec<SourceFence>,
        available_bytes: u64,
        protected_floor: u64,
    ) -> EffectAuthorization {
        EffectAuthorization::Mutating {
            live_intent: LiveIntentAuthorization {
                owner: self.shared.owner,
                nonce: 1,
                source_fences,
            },
            live_capacity: LiveCapacityAuthorization {
                owner: self.shared.owner,
                nonce: 1,
                available_bytes,
                protected_floor,
            },
        }
    }

    #[cfg(test)]
    fn test_observation_authorization(
        &self,
        registration: RootRegistrationRef,
        source_fences: Vec<SourceFence>,
    ) -> EffectAuthorization {
        EffectAuthorization::ObservationOnly {
            live_root_registration: registration,
            source_fences,
        }
    }

    #[cfg(test)]
    fn panic_for_test(&self) -> Result<(), FileOperationOwnerQueueError> {
        self.shared.try_control(OwnerCommand::Panic { input: None })
    }

    #[cfg(test)]
    fn acknowledge_custody_for_test_with_owner(&self, owner: OwnerIdentity) {
        let admissions = self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let all_terminal = admissions
            .values()
            .all(|admission| matches!(admission.state, AdmissionState::Completed));
        let custody = if all_terminal {
            Some(CustodyAck {
                owner,
                retained: admissions
                    .iter()
                    .map(|(key, admission)| (*key, admission.fingerprint))
                    .collect(),
            })
        } else {
            None
        };
        *self
            .shared
            .custody_ack
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = custody;
    }

    #[cfg(test)]
    fn acknowledge_custody_for_test(&self) {
        self.acknowledge_custody_for_test_with_owner(self.shared.owner);
    }

    #[cfg(test)]
    fn retained_evidence_for_test(
        &self,
    ) -> Vec<(PendingEffectIdentity, usize, RequestFingerprint)> {
        let admissions = self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let retained = self
            .shared
            .retained_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut evidence = BTreeMap::new();
        for (key, admission) in admissions.iter() {
            evidence.insert(
                key.observation(),
                (admission.canonical.len(), admission.fingerprint),
            );
        }
        if let Some(state) = retained.as_ref() {
            for (key, receipt) in state.receipts.iter() {
                evidence.insert(
                    key.observation(),
                    (receipt.canonical.len(), receipt.fingerprint),
                );
            }
            for (key, tombstone) in state.tombstones.iter() {
                evidence.insert(
                    key.observation(),
                    (tombstone.canonical.len(), tombstone.fingerprint),
                );
            }
        }
        evidence
            .into_iter()
            .map(|(identity, (bytes, fingerprint))| (identity, bytes, fingerprint))
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn execution_count_for_test(&self) -> usize {
        self.shared.execution_count.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn force_next_live_chunk_uncertain_for_test(&self) {
        self.shared
            .force_live_chunk_uncertain
            .store(true, Ordering::Release);
    }

    #[cfg(test)]
    fn pause_before_execute_for_test(&self) {
        *self
            .shared
            .pause_reached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = false;
        *self
            .shared
            .pause_before_execute
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
    }

    #[cfg(test)]
    fn wait_until_worker_paused_for_test(&self) {
        let mut reached = self
            .shared
            .pause_reached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while !*reached {
            reached = self
                .shared
                .pause_reached_cv
                .wait(reached)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    #[cfg(test)]
    fn pause_after_execution_start_for_test(&self) {
        *self
            .shared
            .execution_started
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = false;
        *self
            .shared
            .pause_after_execution_start
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
    }

    #[cfg(test)]
    fn wait_until_execution_started_for_test(&self) {
        let mut started = self
            .shared
            .execution_started
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while !*started {
            started = self
                .shared
                .execution_cv
                .wait(started)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    #[cfg(test)]
    fn release_after_execution_start_for_test(&self) {
        *self
            .shared
            .pause_after_execution_start
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = false;
        self.shared.execution_pause_cv.notify_all();
    }

    #[cfg(test)]
    fn pause_before_retained_shutdown_outcome_for_test(&self) {
        *self
            .shared
            .retained_shutdown_outcome_reached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = false;
        *self
            .shared
            .pause_before_retained_shutdown_outcome
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
    }

    #[cfg(test)]
    fn input_bytes_remaining_for_test(&self) -> usize {
        *self
            .shared
            .input_remaining
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[cfg(test)]
    fn release_worker_for_test(&self) {
        *self
            .shared
            .pause_before_execute
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = false;
        self.shared.pause_cv.notify_all();
    }
}

impl Drop for FileOperationOwner {
    fn drop(&mut self) {
        let had_worker = self
            .join
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .is_some();
        if had_worker {
            self.shared.implicit_drop.store(true, Ordering::Release);
            self.shared
                .mark_unavailable("owner dropped without orderly shutdown");
            self.shared.close_commands();
        } else if OwnerLifecycle::from_u8(self.shared.lifecycle.load(Ordering::Acquire))
            == OwnerLifecycle::Draining
        {
            self.shared
                .mark_unavailable("owner dropped with deferred evidence");
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct EffectKey {
    operation_id: OperationId,
    participant_id: ParticipantId,
    effect_id: EffectId,
}

impl EffectKey {
    fn from_command(command: &FileOperationCommand) -> Self {
        Self {
            operation_id: command.operation_id,
            participant_id: command.participant_id,
            effect_id: command.effect_id,
        }
    }

    fn observation(self) -> PendingEffectIdentity {
        PendingEffectIdentity {
            operation_id: self.operation_id,
            participant_id: self.participant_id,
            effect_id: self.effect_id,
        }
    }
}

struct ReceiptEntry {
    canonical: Arc<[u8]>,
    fingerprint: RequestFingerprint,
    result: Arc<FileOperationResult>,
    aliases: usize,
    grant: Option<LiveChunkEffectGrant>,
}

struct ReceiptTombstone {
    canonical: Arc<[u8]>,
    fingerprint: RequestFingerprint,
    result: Arc<FileOperationResult>,
    aliases: usize,
    grant: Option<LiveChunkEffectGrant>,
}

enum AdmissionState {
    Queued,
    Executing,
    Completed,
}

enum ExecutionStart {
    Started,
    Cancelled,
    AlreadyStarted,
    Missing,
}

struct AdmissionRecord {
    canonical: Arc<[u8]>,
    fingerprint: RequestFingerprint,
    input: InputPermit,
    state: AdmissionState,
    cancel_requested: bool,
}

struct RootRegistrationState {
    authority: AuthorityFence,
    _label: String,
    active: bool,
}

struct WorkerState {
    registrations: BTreeMap<RootRegistrationRef, RootRegistrationState>,
    receipts: BTreeMap<EffectKey, ReceiptEntry>,
    tombstones: BTreeMap<EffectKey, ReceiptTombstone>,
}

impl WorkerState {
    fn new() -> Self {
        Self {
            registrations: BTreeMap::new(),
            receipts: BTreeMap::new(),
            tombstones: BTreeMap::new(),
        }
    }

    #[cfg(test)]
    fn retained_identities(&self) -> BTreeMap<EffectKey, RequestFingerprint> {
        self.receipts
            .iter()
            .map(|(key, entry)| (*key, entry.fingerprint))
            .chain(
                self.tombstones
                    .iter()
                    .map(|(key, entry)| (*key, entry.fingerprint)),
            )
            .collect()
    }

    fn handle(&mut self, command: OwnerCommand, shared: &Arc<OwnerShared>) {
        match command {
            OwnerCommand::Execute {
                command,
                grant,
                canonical,
                fingerprint,
                key,
                input,
                primary,
                result,
            } => self.execute(
                command,
                grant,
                canonical,
                fingerprint,
                key,
                input,
                primary,
                result,
                shared,
            ),
            OwnerCommand::Acknowledge {
                key,
                fingerprint,
                input,
            } => {
                self.acknowledge(key, fingerprint, input);
            }
            OwnerCommand::TakeUnusedLiveChunkGrant {
                key,
                fingerprint,
                result,
            } => {
                self.take_unused_live_chunk_grant(key, fingerprint, result);
            }
            #[cfg(test)]
            OwnerCommand::RegisterRoot {
                registration,
                authority,
                label,
                input,
                result,
            } => {
                if registration.owner != shared.owner {
                    let _ = result.send(Err(String::from("registration belongs to another owner")));
                    drop(input);
                    return;
                }
                let response = match self.registrations.get(&registration) {
                    Some(existing)
                        if existing.authority == authority && existing._label == label =>
                    {
                        Ok(())
                    }
                    Some(_) => Err(String::from("conflicting root registration")),
                    None => {
                        self.registrations.insert(
                            registration,
                            RootRegistrationState {
                                authority,
                                _label: label,
                                active: true,
                            },
                        );
                        Ok(())
                    }
                };
                let _ = result.send(response);
                drop(input);
            }
            #[cfg(test)]
            OwnerCommand::RetireRoot {
                registration,
                input,
                result,
            } => {
                let response = match self.registrations.get_mut(&registration) {
                    Some(existing) => {
                        existing.active = false;
                        Ok(())
                    }
                    None => Err(String::from("root registration is not retained")),
                };
                let _ = result.send(response);
                drop(input);
            }
            #[cfg(test)]
            OwnerCommand::Panic { input } => {
                drop(input);
                panic!("synthetic file-operation owner panic");
            }
        }
    }

    fn execute(
        &mut self,
        command: FileOperationCommand,
        grant: Option<LiveChunkEffectGrant>,
        canonical: Arc<[u8]>,
        fingerprint: RequestFingerprint,
        key: EffectKey,
        input: Option<InputPermit>,
        primary: bool,
        result_tx: SyncSender<Arc<FileOperationResult>>,
        shared: &Arc<OwnerShared>,
    ) {
        if let Some(existing) = self.receipts.get_mut(&key) {
            if existing.canonical.as_ref() == canonical.as_ref() {
                let result = existing.result.clone();
                if result_tx.send(result).is_ok() {
                    existing.aliases = existing.aliases.saturating_add(1);
                }
            } else {
                let conflict =
                    Arc::new(conflict_result(&command, fingerprint, existing.fingerprint));
                let _ = result_tx.send(conflict);
            }
            drop(input);
            return;
        }
        if let Some(existing) = self.tombstones.get_mut(&key) {
            if existing.canonical.as_ref() == canonical.as_ref() {
                let result = existing.result.clone();
                if result_tx.send(result).is_ok() {
                    existing.aliases = existing.aliases.saturating_add(1);
                }
            } else {
                let conflict =
                    Arc::new(conflict_result(&command, fingerprint, existing.fingerprint));
                let _ = result_tx.send(conflict);
            }
            drop(input);
            return;
        }
        if self.receipts.len() + self.tombstones.len() >= FILE_OPERATION_OWNER_MAX_RECEIPTS {
            let result = Arc::new(rejection_result(
                &command,
                fingerprint,
                EffectOutcome::RetryableBeforeEffect {
                    reason: String::from("unacknowledged effect receipt capacity is full"),
                },
            ));
            let _ = result_tx.send(result);
            drop(input);
            return;
        }
        let outcome = if primary {
            #[cfg(test)]
            shared.wait_for_test_release();
            match shared.begin_execution(key, fingerprint) {
                ExecutionStart::Cancelled => EffectOutcome::SupersededBeforeEffect {
                    reason: String::from("effect was cancelled before its execution boundary"),
                },
                ExecutionStart::Started => {
                    #[cfg(test)]
                    shared.wait_for_execution_start();
                    #[cfg(test)]
                    let forced_uncertain = grant.is_some()
                        && shared
                            .force_live_chunk_uncertain
                            .swap(false, Ordering::AcqRel);
                    #[cfg(not(test))]
                    let forced_uncertain = false;
                    if forced_uncertain {
                        EffectOutcome::RetainedUncertain {
                            reason: String::from("synthetic lost effect boundary"),
                        }
                    } else {
                        match validate_for_owner(
                            &command,
                            grant.as_ref(),
                            shared,
                            &self.registrations,
                        ) {
                            Ok(()) => match grant
                                .as_ref()
                                .map(LiveChunkEffectGrant::begin_source_effect)
                                .transpose()
                            {
                                Err(_) => EffectOutcome::SupersededBeforeEffect {
                                    reason: String::from(
                                        "live source authority closed before effect",
                                    ),
                                },
                                Ok(_source_permit) => {
                                    #[cfg(test)]
                                    shared.execution_count.fetch_add(1, Ordering::AcqRel);
                                    EffectOutcome::Unsupported {
                                        reason: String::from(
                                            "no production file-effect producer or qualified backend is available",
                                        ),
                                    }
                                }
                            },
                            Err(outcome) => outcome,
                        }
                    }
                }
                ExecutionStart::AlreadyStarted | ExecutionStart::Missing => {
                    EffectOutcome::RetainedUncertain {
                        reason: String::from("accepted effect admission boundary was lost"),
                    }
                }
            }
        } else if self.receipts.contains_key(&key) || self.tombstones.contains_key(&key) {
            // FIFO ordering normally means the primary command has already installed its
            // receipt.  Keep a conservative result if an adapter ever violates that ordering.
            return self.execute_duplicate_or_conflict(
                command,
                canonical,
                fingerprint,
                key,
                input,
                result_tx,
            );
        } else {
            EffectOutcome::RetryableBeforeEffect {
                reason: String::from("duplicate arrived before its primary admission result"),
            }
        };
        drop(input);
        let result = Arc::new(rejection_result(&command, fingerprint, outcome));
        shared.complete_admission(key);
        let aliases = usize::from(result_tx.send(Arc::clone(&result)).is_ok());
        self.receipts.insert(
            key,
            ReceiptEntry {
                canonical,
                fingerprint,
                result,
                aliases,
                grant,
            },
        );
    }

    fn execute_duplicate_or_conflict(
        &mut self,
        command: FileOperationCommand,
        canonical: Arc<[u8]>,
        fingerprint: RequestFingerprint,
        key: EffectKey,
        input: Option<InputPermit>,
        result_tx: SyncSender<Arc<FileOperationResult>>,
    ) {
        if let Some(existing) = self.receipts.get_mut(&key) {
            if existing.canonical.as_ref() == canonical.as_ref() {
                if result_tx.send(existing.result.clone()).is_ok() {
                    existing.aliases = existing.aliases.saturating_add(1);
                }
            } else {
                let _ = result_tx.send(Arc::new(conflict_result(
                    &command,
                    fingerprint,
                    existing.fingerprint,
                )));
            }
        } else if let Some(existing) = self.tombstones.get_mut(&key) {
            if existing.canonical.as_ref() == canonical.as_ref() {
                if result_tx.send(existing.result.clone()).is_ok() {
                    existing.aliases = existing.aliases.saturating_add(1);
                }
            } else {
                let _ = result_tx.send(Arc::new(conflict_result(
                    &command,
                    fingerprint,
                    existing.fingerprint,
                )));
            }
        }
        drop(input);
    }

    fn acknowledge(&mut self, key: EffectKey, fingerprint: RequestFingerprint, input: InputPermit) {
        if let Some(existing) = self.receipts.get_mut(&key) {
            if existing.fingerprint == fingerprint && existing.aliases > 0 {
                existing.aliases -= 1;
                if existing.aliases == 0 {
                    let entry = self.receipts.remove(&key).expect("receipt entry exists");
                    self.tombstones.insert(
                        key,
                        ReceiptTombstone {
                            canonical: entry.canonical,
                            fingerprint: entry.fingerprint,
                            result: entry.result,
                            aliases: 0,
                            grant: entry.grant,
                        },
                    );
                }
            }
        } else if let Some(existing) = self.tombstones.get_mut(&key) {
            if existing.fingerprint == fingerprint && existing.aliases > 0 {
                existing.aliases -= 1;
            }
        }
        drop(input);
    }

    fn take_unused_live_chunk_grant(
        &mut self,
        key: EffectKey,
        fingerprint: RequestFingerprint,
        result: SyncSender<Option<LiveChunkEffectGrant>>,
    ) {
        let entry = if let Some(entry) = self.receipts.get_mut(&key) {
            Some((&entry.fingerprint, &entry.result, &mut entry.grant))
        } else {
            self.tombstones
                .get_mut(&key)
                .map(|entry| (&entry.fingerprint, &entry.result, &mut entry.grant))
        };
        let Some((retained_fingerprint, receipt, grant)) = entry else {
            let _ = result.send(None);
            return;
        };
        let confirmed_no_effect = matches!(
            receipt.outcome(),
            EffectOutcome::SupersededBeforeEffect { .. }
                | EffectOutcome::Collision { .. }
                | EffectOutcome::Unsupported { .. }
                | EffectOutcome::RetryableBeforeEffect { .. }
        );
        if *retained_fingerprint != fingerprint || !confirmed_no_effect {
            let _ = result.send(None);
            return;
        }
        if let Err(error) = result.send(grant.take()) {
            *grant = error.0;
        }
    }
}

enum OwnerCommand {
    Execute {
        command: FileOperationCommand,
        grant: Option<LiveChunkEffectGrant>,
        canonical: Arc<[u8]>,
        fingerprint: RequestFingerprint,
        key: EffectKey,
        input: Option<InputPermit>,
        primary: bool,
        result: SyncSender<Arc<FileOperationResult>>,
    },
    Acknowledge {
        key: EffectKey,
        fingerprint: RequestFingerprint,
        input: InputPermit,
    },
    TakeUnusedLiveChunkGrant {
        key: EffectKey,
        fingerprint: RequestFingerprint,
        result: SyncSender<Option<LiveChunkEffectGrant>>,
    },
    #[cfg(test)]
    RegisterRoot {
        registration: RootRegistrationRef,
        authority: AuthorityFence,
        label: String,
        input: Option<InputPermit>,
        result: SyncSender<Result<(), String>>,
    },
    #[cfg(test)]
    RetireRoot {
        registration: RootRegistrationRef,
        input: Option<InputPermit>,
        result: SyncSender<Result<(), String>>,
    },
    #[cfg(test)]
    Panic { input: Option<InputPermit> },
}

struct OwnerShared {
    owner: OwnerIdentity,
    commands: Mutex<Option<SyncSender<OwnerCommand>>>,
    admission_gate: Mutex<()>,
    admissions: Mutex<BTreeMap<EffectKey, AdmissionRecord>>,
    retained_state: Mutex<Option<WorkerState>>,
    lifecycle: AtomicU8,
    unavailable: Mutex<Option<String>>,
    input_remaining: Mutex<usize>,
    results_remaining: Mutex<usize>,
    explicit_close: AtomicBool,
    implicit_drop: AtomicBool,
    #[cfg(test)]
    custody_ack: Mutex<Option<CustodyAck>>,
    shutdown_outcome: Mutex<Option<FileOperationOwnerShutdown>>,
    #[cfg(test)]
    execution_count: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    force_live_chunk_uncertain: AtomicBool,
    #[cfg(test)]
    pause_before_execute: Mutex<bool>,
    #[cfg(test)]
    pause_reached: Mutex<bool>,
    #[cfg(test)]
    pause_after_execution_start: Mutex<bool>,
    #[cfg(test)]
    execution_started: Mutex<bool>,
    #[cfg(test)]
    pause_before_retained_shutdown_outcome: Mutex<bool>,
    #[cfg(test)]
    retained_shutdown_outcome_reached: Mutex<bool>,
    #[cfg(test)]
    execution_cv: Condvar,
    #[cfg(test)]
    execution_pause_cv: Condvar,
    #[cfg(test)]
    retained_shutdown_outcome_cv: Condvar,
    #[cfg(test)]
    retained_shutdown_outcome_release_cv: Condvar,
    #[cfg(test)]
    pause_reached_cv: Condvar,
    #[cfg(test)]
    pause_cv: Condvar,
}

#[cfg(test)]
struct CustodyAck {
    owner: OwnerIdentity,
    retained: BTreeMap<EffectKey, RequestFingerprint>,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OwnerLifecycle {
    Initializing = 0,
    Running = 1,
    Draining = 2,
    Stopped = 3,
    Unavailable = 4,
}

impl OwnerLifecycle {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Running,
            2 => Self::Draining,
            3 => Self::Stopped,
            4 => Self::Unavailable,
            _ => Self::Initializing,
        }
    }
}

impl OwnerShared {
    fn new(owner: OwnerIdentity, command_sender: SyncSender<OwnerCommand>) -> Self {
        Self {
            owner,
            commands: Mutex::new(Some(command_sender)),
            admission_gate: Mutex::new(()),
            admissions: Mutex::new(BTreeMap::new()),
            retained_state: Mutex::new(None),
            lifecycle: AtomicU8::new(OwnerLifecycle::Initializing as u8),
            unavailable: Mutex::new(None),
            input_remaining: Mutex::new(FILE_OPERATION_OWNER_MAX_INPUT_BYTES),
            results_remaining: Mutex::new(FILE_OPERATION_OWNER_RESULT_CAPACITY),
            explicit_close: AtomicBool::new(false),
            implicit_drop: AtomicBool::new(false),
            shutdown_outcome: Mutex::new(None),
            #[cfg(test)]
            execution_count: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            force_live_chunk_uncertain: AtomicBool::new(false),
            #[cfg(test)]
            custody_ack: Mutex::new(None),
            #[cfg(test)]
            pause_before_execute: Mutex::new(false),
            #[cfg(test)]
            pause_reached: Mutex::new(false),
            #[cfg(test)]
            pause_after_execution_start: Mutex::new(false),
            #[cfg(test)]
            execution_started: Mutex::new(false),
            #[cfg(test)]
            pause_before_retained_shutdown_outcome: Mutex::new(false),
            #[cfg(test)]
            retained_shutdown_outcome_reached: Mutex::new(false),
            #[cfg(test)]
            execution_cv: Condvar::new(),
            #[cfg(test)]
            execution_pause_cv: Condvar::new(),
            #[cfg(test)]
            retained_shutdown_outcome_cv: Condvar::new(),
            #[cfg(test)]
            retained_shutdown_outcome_release_cv: Condvar::new(),
            #[cfg(test)]
            pause_reached_cv: Condvar::new(),
            #[cfg(test)]
            pause_cv: Condvar::new(),
        }
    }

    fn status(&self) -> FileOperationOwnerStatus {
        match OwnerLifecycle::from_u8(self.lifecycle.load(Ordering::Acquire)) {
            OwnerLifecycle::Running => FileOperationOwnerStatus::Running,
            OwnerLifecycle::Draining => FileOperationOwnerStatus::Draining,
            OwnerLifecycle::Stopped => FileOperationOwnerStatus::Stopped,
            OwnerLifecycle::Unavailable | OwnerLifecycle::Initializing => {
                FileOperationOwnerStatus::Unavailable {
                    reason: self
                        .unavailable
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clone()
                        .unwrap_or_else(|| String::from("owner is unavailable")),
                }
            }
        }
    }

    fn ensure_accepting(&self) -> Result<(), FileOperationOwnerQueueError> {
        match OwnerLifecycle::from_u8(self.lifecycle.load(Ordering::Acquire)) {
            OwnerLifecycle::Running => Ok(()),
            OwnerLifecycle::Draining => Err(FileOperationOwnerQueueError::Draining),
            OwnerLifecycle::Stopped => Err(FileOperationOwnerQueueError::Stopped),
            OwnerLifecycle::Unavailable | OwnerLifecycle::Initializing => {
                Err(FileOperationOwnerQueueError::Unavailable(
                    self.unavailable
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clone()
                        .unwrap_or_else(|| String::from("owner is unavailable")),
                ))
            }
        }
    }

    fn reserve_input(
        self: &Arc<Self>,
        requested: usize,
    ) -> Result<InputPermit, FileOperationOwnerQueueError> {
        let mut remaining = self
            .input_remaining
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if requested > *remaining {
            return Err(FileOperationOwnerQueueError::InputBytesFull { requested });
        }
        *remaining -= requested;
        Ok(InputPermit {
            shared: Arc::downgrade(self),
            bytes: requested,
        })
    }

    fn refund_input(&self, bytes: usize) {
        let mut remaining = self
            .input_remaining
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *remaining = remaining
            .saturating_add(bytes)
            .min(FILE_OPERATION_OWNER_MAX_INPUT_BYTES);
    }

    fn reserve_result(self: &Arc<Self>) -> Result<ResultPermit, FileOperationOwnerQueueError> {
        let mut remaining = self
            .results_remaining
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *remaining == 0 {
            return Err(FileOperationOwnerQueueError::ResultsFull);
        }
        *remaining -= 1;
        Ok(ResultPermit {
            shared: Arc::downgrade(self),
        })
    }

    fn refund_result(&self) {
        let mut remaining = self
            .results_remaining
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *remaining = remaining
            .saturating_add(1)
            .min(FILE_OPERATION_OWNER_RESULT_CAPACITY);
    }

    fn begin_drain(&self) -> Result<(), FileOperationOwnerQueueError> {
        let _gate = self
            .admission_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.transition_to_draining()
    }

    fn close_commands(&self) {
        let _gate = self
            .admission_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = self.transition_to_draining();
        self.commands
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
    }

    fn mark_unavailable(&self, reason: &str) {
        *self
            .unavailable
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(bounded_diagnostic(reason.to_owned()));
        loop {
            let current = self.lifecycle.load(Ordering::Acquire);
            let current_lifecycle = OwnerLifecycle::from_u8(current);
            match current_lifecycle {
                OwnerLifecycle::Unavailable | OwnerLifecycle::Stopped => return,
                OwnerLifecycle::Initializing
                | OwnerLifecycle::Running
                | OwnerLifecycle::Draining => {
                    if self
                        .lifecycle
                        .compare_exchange(
                            current,
                            OwnerLifecycle::Unavailable as u8,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        self.replace_clean_shutdown_with_uncertain(reason);
                        return;
                    }
                }
            }
        }
    }

    fn transition_to_draining(&self) -> Result<(), FileOperationOwnerQueueError> {
        loop {
            let current = self.lifecycle.load(Ordering::Acquire);
            match OwnerLifecycle::from_u8(current) {
                OwnerLifecycle::Running => {
                    if self
                        .lifecycle
                        .compare_exchange(
                            current,
                            OwnerLifecycle::Draining as u8,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return Ok(());
                    }
                }
                OwnerLifecycle::Draining => return Ok(()),
                OwnerLifecycle::Stopped => return Err(FileOperationOwnerQueueError::Stopped),
                OwnerLifecycle::Unavailable | OwnerLifecycle::Initializing => {
                    return Err(FileOperationOwnerQueueError::Unavailable(
                        self.unavailable
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .clone()
                            .unwrap_or_else(|| String::from("owner is unavailable")),
                    ));
                }
            }
        }
    }

    fn try_stop(&self) -> bool {
        loop {
            let current = self.lifecycle.load(Ordering::Acquire);
            match OwnerLifecycle::from_u8(current) {
                OwnerLifecycle::Stopped => return true,
                OwnerLifecycle::Unavailable | OwnerLifecycle::Initializing => return false,
                OwnerLifecycle::Running | OwnerLifecycle::Draining => {
                    if self
                        .lifecycle
                        .compare_exchange(
                            current,
                            OwnerLifecycle::Stopped as u8,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return true;
                    }
                }
            }
        }
    }

    fn replace_clean_shutdown_with_uncertain(&self, reason: &str) {
        let pending = self.pending_effects();
        let mut outcome = self
            .shutdown_outcome
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if matches!(
            outcome.as_ref(),
            Some(
                FileOperationOwnerShutdown::DrainedAck
                    | FileOperationOwnerShutdown::RetirementDeferred { .. }
            )
        ) {
            *outcome = Some(FileOperationOwnerShutdown::RetirementUncertain {
                pending,
                reason: bounded_diagnostic(reason.to_owned()),
            });
        }
    }

    fn set_retained_shutdown_outcome(&self, pending: Vec<PendingEffectIdentity>, reason: &str) {
        let mut outcome = self
            .shutdown_outcome
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let unavailable = OwnerLifecycle::from_u8(self.lifecycle.load(Ordering::Acquire))
            == OwnerLifecycle::Unavailable;
        *outcome = Some(if unavailable {
            FileOperationOwnerShutdown::RetirementUncertain {
                pending,
                reason: bounded_diagnostic(reason.to_owned()),
            }
        } else {
            FileOperationOwnerShutdown::RetirementDeferred {
                pending,
                reason: bounded_diagnostic(reason.to_owned()),
            }
        });
    }

    fn begin_execution(&self, key: EffectKey, fingerprint: RequestFingerprint) -> ExecutionStart {
        let mut admissions = self
            .admissions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(admission) = admissions.get_mut(&key) else {
            return ExecutionStart::Missing;
        };
        if admission.fingerprint != fingerprint {
            return ExecutionStart::Missing;
        }
        if !matches!(admission.state, AdmissionState::Queued) {
            return ExecutionStart::AlreadyStarted;
        }
        if admission.cancel_requested {
            admission.state = AdmissionState::Completed;
            return ExecutionStart::Cancelled;
        }
        admission.state = AdmissionState::Executing;
        ExecutionStart::Started
    }

    fn complete_admission(&self, key: EffectKey) {
        if let Some(admission) = self
            .admissions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get_mut(&key)
        {
            admission.state = AdmissionState::Completed;
        }
    }

    fn pending_effects(&self) -> Vec<PendingEffectIdentity> {
        self.admissions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|(key, admission)| {
                let _retained_bytes = admission
                    .canonical
                    .len()
                    .saturating_add(admission.input.bytes);
                (*key).observation()
            })
            .collect()
    }

    #[cfg(test)]
    fn custody_ack_matches(&self, state: &WorkerState) -> bool {
        let custody_ack = self
            .custody_ack
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(ack) = custody_ack.as_ref() else {
            return false;
        };
        ack.owner == self.owner && ack.retained == state.retained_identities()
    }

    #[cfg(not(test))]
    fn custody_ack_matches(&self, _state: &WorkerState) -> bool {
        false
    }

    fn retain_worker_state(&self, state: WorkerState) {
        *self
            .retained_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(state);
    }

    fn clear_admissions(&self) {
        self.admissions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    #[cfg(test)]
    fn wait_for_test_release(&self) {
        let mut paused = self
            .pause_before_execute
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while *paused {
            *self
                .pause_reached
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
            self.pause_reached_cv.notify_all();
            paused = self
                .pause_cv
                .wait(paused)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    #[cfg(test)]
    fn wait_for_execution_start(&self) {
        {
            let mut started = self
                .execution_started
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *started = true;
        }
        self.execution_cv.notify_all();
        let mut paused = self
            .pause_after_execution_start
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while *paused {
            paused = self
                .execution_pause_cv
                .wait(paused)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    #[cfg(test)]
    fn wait_for_retained_shutdown_outcome(&self) {
        let mut paused = self
            .pause_before_retained_shutdown_outcome
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !*paused {
            return;
        }
        *self
            .retained_shutdown_outcome_reached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
        self.retained_shutdown_outcome_cv.notify_all();
        while *paused {
            paused = self
                .retained_shutdown_outcome_release_cv
                .wait(paused)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    #[cfg(test)]
    fn try_control(&self, command: OwnerCommand) -> Result<(), FileOperationOwnerQueueError> {
        let _gate = self
            .admission_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.ensure_accepting()?;
        let sender = self
            .commands
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .cloned()
            .ok_or(FileOperationOwnerQueueError::Stopped)?;
        match sender.try_send(command) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(FileOperationOwnerQueueError::Full),
            Err(TrySendError::Disconnected(_)) => {
                self.mark_unavailable("owner command channel disconnected");
                Err(FileOperationOwnerQueueError::Unavailable(String::from(
                    "owner command channel disconnected",
                )))
            }
        }
    }

    fn try_cancel(
        &self,
        key: EffectKey,
    ) -> Result<FileOperationCancelOutcome, FileOperationOwnerQueueError> {
        let _gate = self
            .admission_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut admissions = self
            .admissions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(admission) = admissions.get_mut(&key) else {
            return Ok(FileOperationCancelOutcome::Unknown);
        };
        match admission.state {
            AdmissionState::Queued => {
                admission.cancel_requested = true;
                Ok(FileOperationCancelOutcome::KnownPending)
            }
            AdmissionState::Executing => Ok(FileOperationCancelOutcome::AlreadyStarted),
            AdmissionState::Completed => Ok(FileOperationCancelOutcome::AlreadyCompleted),
        }
    }

    fn try_acknowledge(
        self: &Arc<Self>,
        key: EffectKey,
        fingerprint: RequestFingerprint,
    ) -> Result<(), FileOperationRequestError> {
        let input = self
            .reserve_input(1)
            .map_err(FileOperationRequestError::Acknowledgement)?;
        let command = OwnerCommand::Acknowledge {
            key,
            fingerprint,
            input,
        };
        let sender = self
            .commands
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .cloned()
            .ok_or_else(|| {
                FileOperationRequestError::Acknowledgement(FileOperationOwnerQueueError::Stopped)
            });
        let sender = match sender {
            Ok(sender) => sender,
            Err(error) => return Err(error),
        };
        match sender.try_send(command) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(command)) => {
                drop(command);
                Err(FileOperationRequestError::Acknowledgement(
                    FileOperationOwnerQueueError::Full,
                ))
            }
            Err(TrySendError::Disconnected(command)) => {
                drop(command);
                Err(FileOperationRequestError::Acknowledgement(
                    FileOperationOwnerQueueError::Unavailable(String::from(
                        "owner command channel disconnected",
                    )),
                ))
            }
        }
    }

    fn try_take_unused_live_chunk_grant(
        &self,
        key: EffectKey,
        fingerprint: RequestFingerprint,
    ) -> Result<Option<LiveChunkEffectGrant>, FileOperationOwnerQueueError> {
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let command = OwnerCommand::TakeUnusedLiveChunkGrant {
            key,
            fingerprint,
            result: result_tx,
        };
        let sender = self
            .commands
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .cloned()
            .ok_or(FileOperationOwnerQueueError::Stopped)?;
        match sender.try_send(command) {
            Ok(()) => result_rx
                .recv()
                .map_err(|_| FileOperationOwnerQueueError::Stopped),
            Err(TrySendError::Full(_)) => Err(FileOperationOwnerQueueError::Full),
            Err(TrySendError::Disconnected(_)) => Err(FileOperationOwnerQueueError::Unavailable(
                String::from("owner command channel disconnected"),
            )),
        }
    }
}

fn normalize_string(value: String) -> String {
    value.into_boxed_str().into()
}

struct InputPermit {
    shared: Weak<OwnerShared>,
    bytes: usize,
}

impl Drop for InputPermit {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.upgrade() {
            shared.refund_input(self.bytes);
        }
    }
}

struct ResultPermit {
    shared: Weak<OwnerShared>,
}

impl Drop for ResultPermit {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.upgrade() {
            shared.refund_result();
        }
    }
}

fn worker_main(
    shared: Arc<OwnerShared>,
    receiver: Receiver<OwnerCommand>,
    ready: SyncSender<Result<(), String>>,
) {
    let mut state = WorkerState::new();
    let run = panic::catch_unwind(AssertUnwindSafe(|| {
        shared
            .lifecycle
            .store(OwnerLifecycle::Running as u8, Ordering::Release);
        let _ = ready.send(Ok(()));
        while let Ok(command) = receiver.recv() {
            state.handle(command, &shared);
        }
    }));
    match run {
        Ok(()) => finish_worker(shared, state),
        Err(_) => {
            let _ = ready.send(Err(String::from(
                "owner panicked during startup or execution",
            )));
            shared.retain_worker_state(state);
            shared.mark_unavailable("owner worker panicked");
            let outcome = FileOperationOwnerShutdown::RetirementUncertain {
                pending: shared.pending_effects(),
                reason: String::from("owner worker panicked before durable custody"),
            };
            *shared
                .shutdown_outcome
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(outcome);
        }
    }
}

fn finish_worker(shared: Arc<OwnerShared>, mut state: WorkerState) {
    let unavailable = OwnerLifecycle::from_u8(shared.lifecycle.load(Ordering::Acquire))
        == OwnerLifecycle::Unavailable;
    if shared.implicit_drop.load(Ordering::Acquire) || unavailable {
        shared.retain_worker_state(state);
        if shared.implicit_drop.load(Ordering::Acquire) {
            shared.mark_unavailable("owner dropped without orderly shutdown");
        }
        let outcome = FileOperationOwnerShutdown::RetirementUncertain {
            pending: shared.pending_effects(),
            reason: if shared.implicit_drop.load(Ordering::Acquire) {
                String::from("owner dropped before custody acknowledgement")
            } else {
                String::from("owner became unavailable before custody acknowledgement")
            },
        };
        *shared
            .shutdown_outcome
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(outcome);
        return;
    }

    let has_evidence = !state.receipts.is_empty()
        || !state.tombstones.is_empty()
        || !shared.pending_effects().is_empty();
    let custody_ack = shared.custody_ack_matches(&state);
    if custody_ack || !has_evidence {
        if shared.try_stop() {
            state.receipts.clear();
            state.tombstones.clear();
            shared.clear_admissions();
            *shared
                .shutdown_outcome
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                Some(FileOperationOwnerShutdown::DrainedAck);
        } else {
            shared.retain_worker_state(state);
            *shared
                .shutdown_outcome
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                Some(FileOperationOwnerShutdown::RetirementUncertain {
                    pending: shared.pending_effects(),
                    reason: String::from("owner became unavailable during finalization"),
                });
        }
    } else {
        shared.retain_worker_state(state);
        let pending = shared.pending_effects();
        #[cfg(test)]
        shared.wait_for_retained_shutdown_outcome();
        shared.set_retained_shutdown_outcome(
            pending,
            "receipts remain without confirmed coordinator custody",
        );
        // Retained evidence is still live in this owner.  Keep the lifecycle explicitly
        // draining so a deferred shutdown cannot be mistaken for a clean stopped owner.  A
        // concurrent unavailable transition is sticky and converts the outcome to uncertain.
        if OwnerLifecycle::from_u8(shared.lifecycle.load(Ordering::Acquire))
            != OwnerLifecycle::Unavailable
        {
            let _ = shared.transition_to_draining();
        }
    }
}

fn validate_for_owner(
    command: &FileOperationCommand,
    grant: Option<&LiveChunkEffectGrant>,
    shared: &Arc<OwnerShared>,
    registrations: &BTreeMap<RootRegistrationRef, RootRegistrationState>,
) -> Result<(), EffectOutcome> {
    for (_, binding) in command.effect.role_bindings() {
        if binding.registration.owner != shared.owner {
            return Err(EffectOutcome::SupersededBeforeEffect {
                reason: String::from("root registration belongs to another owner"),
            });
        }
        let Some(registration) = registrations.get(&binding.registration) else {
            return Err(EffectOutcome::SupersededBeforeEffect {
                reason: String::from("root registration is not retained by this owner"),
            });
        };
        if !registration.active || registration.authority != binding.authority {
            return Err(EffectOutcome::SupersededBeforeEffect {
                reason: String::from("root authority fence is stale or mismatched"),
            });
        }
    }
    match &command.authorization {
        EffectAuthorization::Mutating {
            live_intent,
            live_capacity,
        } => {
            if live_intent.owner != shared.owner || live_capacity.owner != shared.owner {
                return Err(EffectOutcome::SupersededBeforeEffect {
                    reason: String::from("live authority belongs to another owner"),
                });
            }
            if live_intent.source_fences != command.source_fences {
                return Err(EffectOutcome::SupersededBeforeEffect {
                    reason: String::from("live source fences do not match the request"),
                });
            }
            let required = command.effect.required_bytes();
            let Some(available_after_floor) = live_capacity
                .available_bytes
                .checked_sub(live_capacity.protected_floor)
            else {
                return Err(EffectOutcome::RetryableBeforeEffect {
                    reason: String::from("live capacity is below its protected floor"),
                });
            };
            if required > available_after_floor {
                return Err(EffectOutcome::RetryableBeforeEffect {
                    reason: format!(
                        "live capacity has {available_after_floor} bytes above its protected floor, but {required} are required",
                    ),
                });
            }
        }
        EffectAuthorization::ObservationOnly {
            live_root_registration,
            source_fences,
        } => {
            if *live_root_registration != command.target_registration
                || live_root_registration.owner != shared.owner
                || source_fences != &command.source_fences
            {
                return Err(EffectOutcome::SupersededBeforeEffect {
                    reason: String::from("observation authority does not match the request"),
                });
            }
        }
        EffectAuthorization::JournalGrant(_) => {
            if !grant.is_some_and(|live| live.matches_command(command)) {
                return Err(EffectOutcome::SupersededBeforeEffect {
                    reason: String::from("journal grant no longer matches its exact command"),
                });
            }
        }
    }
    Ok(())
}

fn rejection_result(
    command: &FileOperationCommand,
    fingerprint: RequestFingerprint,
    outcome: EffectOutcome,
) -> FileOperationResult {
    FileOperationResult {
        operation_id: command.operation_id,
        participant_id: command.participant_id,
        effect_id: command.effect_id,
        target_registration: command.target_registration,
        source_fences: command.source_fences.clone(),
        root_bindings: command
            .effect
            .role_bindings()
            .into_iter()
            .map(|(role, binding)| RootBindingObservation {
                role,
                registration: binding.registration,
                authority: binding.authority.clone(),
                locator: binding.locator.clone(),
                expected: binding.expected,
            })
            .collect(),
        expected: command.expected,
        fingerprint,
        outcome,
    }
}

fn conflict_result(
    command: &FileOperationCommand,
    fingerprint: RequestFingerprint,
    original: RequestFingerprint,
) -> FileOperationResult {
    rejection_result(
        command,
        fingerprint,
        EffectOutcome::Conflict {
            original_fingerprint: original,
        },
    )
}

fn require_source_object(binding: &RootRoleBinding) -> Result<(), FileOperationValidationError> {
    if !matches!(binding.authority, AuthorityFence::Source(_)) {
        return Err(FileOperationValidationError::new(
            "source role must carry a source authority fence",
        ));
    }
    if matches!(binding.expected, ExpectedObject::Absent) {
        return Err(FileOperationValidationError::new(
            "source role must carry an exact object identity",
        ));
    }
    Ok(())
}

fn require_present(binding: &RootRoleBinding) -> Result<(), FileOperationValidationError> {
    if matches!(binding.expected, ExpectedObject::Absent) {
        return Err(FileOperationValidationError::new(
            "effect requires an existing object identity",
        ));
    }
    Ok(())
}

fn expected_object(
    binding: &RootRoleBinding,
) -> Result<OwnedObjectIdentity, FileOperationValidationError> {
    match binding.expected {
        ExpectedObject::Exact(object) => Ok(object),
        ExpectedObject::Absent => Err(FileOperationValidationError::new(
            "effect requires an exact object identity",
        )),
    }
}

fn require_binding_object(
    binding: &RootRoleBinding,
    object: OwnedObjectIdentity,
    role: &str,
) -> Result<(), FileOperationValidationError> {
    if expected_object(binding)? != object {
        return Err(FileOperationValidationError::new(format!(
            "{role} identity must match its root role predicate",
        )));
    }
    Ok(())
}

fn require_absent(
    binding: &RootRoleBinding,
    _proof: AbsenceProof,
) -> Result<(), FileOperationValidationError> {
    if binding.expected != ExpectedObject::Absent {
        return Err(FileOperationValidationError::new(
            "no-replace effect requires an absent target predicate",
        ));
    }
    Ok(())
}

fn validate_text(
    field: &str,
    value: &str,
    maximum: usize,
) -> Result<(), FileOperationValidationError> {
    if value.is_empty() || value.len() > maximum || value.contains('\0') {
        return Err(FileOperationValidationError::new(format!(
            "{field} must be non-empty, NUL-free, and at most {maximum} bytes",
        )));
    }
    Ok(())
}

fn bounded_diagnostic(mut value: String) -> String {
    if value.len() <= FILE_OPERATION_OWNER_MAX_DIAGNOSTIC_BYTES {
        return normalize_string(value);
    }
    let mut end = FILE_OPERATION_OWNER_MAX_DIAGNOSTIC_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    normalize_string(value)
}

impl fmt::Display for OwnerIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operation_journal::ParticipantId;

    fn test_object() -> OwnedObjectIdentity {
        OwnedObjectIdentity(Uuid::from_u128(1))
    }

    fn source_fence() -> SourceFence {
        SourceFence::new(
            SourceId::from_string("source-test"),
            "source-root",
            "source-db",
            7,
            Some(11),
            3,
        )
        .expect("valid source fence")
    }

    fn maximum_source_fence(source: char) -> SourceFence {
        let value = source.to_string().repeat(256);
        SourceFence::new(
            SourceId::from_string(value.clone()),
            value.clone(),
            value,
            u64::MAX,
            Some(u64::MAX),
            u64::MAX,
        )
        .expect("maximum source fence")
    }

    fn acknowledge_with_bounded_retry(request: &mut FileOperationRequest) {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match request.acknowledge() {
                Ok(()) => return,
                Err(FileOperationRequestError::Acknowledgement(
                    FileOperationOwnerQueueError::Full,
                )) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("unexpected acknowledgement error: {error:?}"),
            }
        }
    }

    fn profile_fence(owner: OwnerIdentity) -> AuthorityFence {
        AuthorityFence::Profile(ProfileAuthorityFence {
            owner,
            generation: 1,
            lease_epoch: 1,
        })
    }

    fn source_binding(registration: RootRegistrationRef, expected: ExpectedObject) -> SourceRoot {
        SourceRoot {
            binding: RootRoleBinding {
                registration,
                authority: AuthorityFence::Source(source_fence()),
                locator: BoundedRelativeLocator::new("source.wav").expect("locator"),
                expected,
            },
        }
    }

    fn staging_binding(
        registration: RootRegistrationRef,
        owner: OwnerIdentity,
        expected: ExpectedObject,
    ) -> StagingRoot {
        StagingRoot {
            binding: RootRoleBinding {
                registration,
                authority: profile_fence(owner),
                locator: BoundedRelativeLocator::new("stage.bin").expect("locator"),
                expected,
            },
        }
    }

    fn destination_binding(
        registration: RootRegistrationRef,
        owner: OwnerIdentity,
        expected: ExpectedObject,
    ) -> DestinationRoot {
        DestinationRoot {
            binding: RootRoleBinding {
                registration,
                authority: profile_fence(owner),
                locator: BoundedRelativeLocator::new("dest.bin").expect("locator"),
                expected,
            },
        }
    }

    fn owned_binding(registration: RootRegistrationRef, owner: OwnerIdentity) -> OwnedRoot {
        OwnedRoot {
            binding: RootRoleBinding {
                registration,
                authority: profile_fence(owner),
                locator: BoundedRelativeLocator::new("owned.bin").expect("locator"),
                expected: ExpectedObject::Exact(test_object()),
            },
        }
    }

    fn command(
        registration: RootRegistrationRef,
        effect_id: EffectId,
        effect: EffectRequest,
        authorization: EffectAuthorization,
    ) -> FileOperationCommand {
        let target = effect.target_binding();
        FileOperationCommand::for_test(
            OperationId::new(),
            ParticipantId::new(),
            effect_id,
            vec![],
            registration,
            target.expected,
            effect,
            authorization,
        )
    }

    fn unsupported_effect(
        owner: &FileOperationOwner,
        registration: RootRegistrationRef,
        effect_id: EffectId,
    ) -> FileOperationCommand {
        let staging = staging_binding(
            registration,
            owner.owner_identity(),
            ExpectedObject::Exact(test_object()),
        );
        command(
            registration,
            effect_id,
            EffectRequest::FlushStaging {
                staging,
                object: test_object(),
            },
            owner.test_mutating_authorization(vec![], 100, 0),
        )
    }

    #[test]
    fn all_effects_are_typed_and_mutating_effects_fail_closed_without_backend() {
        let owner = FileOperationOwner::start().expect("owner starts");
        let profile = profile_fence(owner.owner_identity());
        let source = AuthorityFence::Source(source_fence());
        let profile_registration = owner
            .register_root_for_test(profile.clone(), "profile")
            .expect("profile registration");
        let destination_registration = owner
            .register_root_for_test(profile, "destination")
            .expect("destination registration");
        let source_registration = owner
            .register_root_for_test(source.clone(), "source")
            .expect("source registration");
        let stage_present = staging_binding(
            profile_registration,
            owner.owner_identity(),
            ExpectedObject::Exact(test_object()),
        );
        let dest_absent = destination_binding(
            destination_registration,
            owner.owner_identity(),
            ExpectedObject::Absent,
        );
        let src = source_binding(source_registration, ExpectedObject::Exact(test_object()));
        let absence = AbsenceProof { marker: 1 };
        let staged = CopyValidatedEvidence {
            object: test_object(),
            bytes: 4,
        };
        let effects = vec![
            EffectRequest::CreateStaging {
                destination: dest_absent.clone(),
                expected_absent: absence,
                bytes: PositiveBoundedBytes::new(1).expect("bytes"),
            },
            EffectRequest::CopyIntoStaging {
                source: src.clone(),
                staging: stage_present.clone(),
                source_range: BoundedRange::new(
                    BoundedOffset::new(0),
                    PositiveBoundedBytes::new(1).expect("bytes"),
                )
                .expect("range"),
                bytes: PositiveBoundedBytes::new(1).expect("bytes"),
            },
            EffectRequest::WriteStagingChunk {
                staging: stage_present.clone(),
                chunk_id: ChunkId::new(),
                issue_id: IssueId::new(),
                output: OutputIdentity::new(),
                offset: BoundedOffset::new(0),
                length: PositiveBoundedBytes::new(1).expect("bytes"),
                payload: BoundedBytes::new(vec![1]).expect("payload"),
            },
            EffectRequest::FlushStaging {
                staging: stage_present.clone(),
                object: test_object(),
            },
            EffectRequest::ValidateStaging {
                staging: stage_present.clone(),
                object: test_object(),
                expected: StagingValidation { expected_bytes: 1 },
            },
            EffectRequest::PublishNoReplace {
                staging: stage_present.clone(),
                target: dest_absent.clone(),
                staged,
                expected_absent: absence,
            },
            EffectRequest::MoveNoReplace {
                source: src.clone(),
                destination: dest_absent.clone(),
                source_object: test_object(),
                expected_absent: absence,
            },
            EffectRequest::CreateDirectoryNoReplace {
                destination: dest_absent.clone(),
                expected_absent: absence,
            },
            EffectRequest::ReplaceExpectedTarget {
                staging: stage_present.clone(),
                target: destination_binding(
                    destination_registration,
                    owner.owner_identity(),
                    ExpectedObject::Exact(test_object()),
                ),
                staged,
                expected_target: test_object(),
            },
            EffectRequest::InspectOwnedObject {
                root: owned_binding(profile_registration, owner.owner_identity()),
                expected: ExpectedObject::Exact(test_object()),
            },
            EffectRequest::RemoveOwnedObject {
                root: owned_binding(profile_registration, owner.owner_identity()),
                expected: test_object(),
                cleanup: CleanupProof { marker: 1 },
            },
            EffectRequest::RestoreOwnedObject {
                root: owned_binding(profile_registration, owner.owner_identity()),
                expected: test_object(),
                restore: RestoreProof { marker: 1 },
            },
        ];
        for effect in effects {
            let target = effect.target_binding().registration;
            let source_fences = effect
                .role_bindings()
                .into_iter()
                .filter_map(|(_, binding)| match &binding.authority {
                    AuthorityFence::Source(fence) => Some(fence.clone()),
                    AuthorityFence::Profile(_) => None,
                })
                .collect::<Vec<_>>();
            let authorization = if matches!(effect, EffectRequest::InspectOwnedObject { .. }) {
                owner.test_observation_authorization(target, vec![])
            } else {
                owner.test_mutating_authorization(source_fences.clone(), 100, 0)
            };
            let mut command = command(target, EffectId::new(), effect, authorization);
            command.source_fences = source_fences;
            if let EffectAuthorization::Mutating { live_intent, .. } = &mut command.authorization {
                live_intent.source_fences = command.source_fences.clone();
            }
            let request = owner.submit(command);
            let mut request = request.expect("typed effect queues");
            let result = request
                .recv_timeout(Duration::from_secs(1))
                .expect("result");
            assert_eq!(result.target_registration(), target);
            assert!(!result.root_bindings().is_empty());
            assert!(matches!(
                result.outcome(),
                EffectOutcome::Unsupported { .. }
            ));
            request.acknowledge().expect("ack");
        }
        assert_eq!(owner.execution_count_for_test(), 12);
        owner.acknowledge_custody_for_test();
        assert_eq!(owner.shutdown(), FileOperationOwnerShutdown::DrainedAck);
    }

    #[test]
    fn exact_duplicate_shares_receipt_and_conflict_keeps_original() {
        let owner = FileOperationOwner::start().expect("owner starts");
        let registration = owner
            .register_root_for_test(profile_fence(owner.owner_identity()), "profile")
            .expect("registration");
        let id = EffectId::new();
        let first_command = unsupported_effect(&owner, registration, id);
        let mut first = owner.submit(first_command.clone_for_test()).expect("first");
        let mut duplicate = owner
            .submit(first_command.clone_for_test())
            .expect("duplicate");
        let first_result = first
            .recv_timeout(Duration::from_secs(1))
            .expect("first result");
        let duplicate_result = duplicate
            .recv_timeout(Duration::from_secs(1))
            .expect("duplicate result");
        assert_eq!(first_result, duplicate_result);
        assert_eq!(owner.execution_count_for_test(), 1);
        first.acknowledge().expect("first ack");
        duplicate.acknowledge().expect("duplicate ack");
        drop(first);
        drop(duplicate);
        let mut changed_command = first_command.clone_for_test();
        changed_command.effect = EffectRequest::CreateDirectoryNoReplace {
            destination: destination_binding(
                owner.owner_identity_ref_for_test(registration),
                owner.owner_identity(),
                ExpectedObject::Absent,
            ),
            expected_absent: AbsenceProof { marker: 2 },
        };
        changed_command.expected = ExpectedObject::Absent;
        let mut changed = owner.submit(changed_command).expect("changed request");
        let result = changed
            .recv_timeout(Duration::from_secs(1))
            .expect("conflict result");
        assert!(matches!(result.outcome(), EffectOutcome::Conflict { .. }));
        changed.acknowledge().expect("conflict ack");
        drop(changed);

        let mut replay = owner
            .submit(first_command.clone_for_test())
            .expect("retained replay");
        assert_eq!(
            replay
                .recv_timeout(Duration::from_secs(1))
                .expect("replay result"),
            first_result
        );
        replay.acknowledge().expect("replay ack");
        drop(replay);

        owner
            .retire_root_for_test(registration)
            .expect("retire root");
        let stale = unsupported_effect(&owner, registration, EffectId::new());
        let mut stale_request = owner.submit(stale).expect("stale command queues");
        let stale_result = stale_request
            .recv_timeout(Duration::from_secs(1))
            .expect("stale result");
        assert!(matches!(
            stale_result.outcome(),
            EffectOutcome::SupersededBeforeEffect { .. }
        ));
        stale_request.acknowledge().expect("stale ack");
        drop(stale_request);

        for _ in 0..(FILE_OPERATION_OWNER_MAX_RECEIPTS - 2) {
            let mut request = owner
                .submit(unsupported_effect(&owner, registration, EffectId::new()))
                .expect("tombstone fill command queues");
            let _ = request
                .recv_timeout(Duration::from_secs(1))
                .expect("fill result");
            request.acknowledge().expect("fill ack");
        }
        assert!(matches!(
            owner.submit(unsupported_effect(&owner, registration, EffectId::new())),
            Err(FileOperationOwnerQueueError::ResultsFull)
        ));
        owner.acknowledge_custody_for_test();
        assert_eq!(owner.shutdown(), FileOperationOwnerShutdown::DrainedAck);
    }

    #[test]
    fn stale_authority_and_observation_only_mutation_fail_before_effect() {
        let owner = FileOperationOwner::start().expect("owner starts");
        let registration = owner
            .register_root_for_test(profile_fence(owner.owner_identity()), "profile")
            .expect("registration");
        let foreign_owner = OwnerIdentity(Uuid::new_v4());
        let foreign_registration = RootRegistrationRef {
            owner: foreign_owner,
            registration_id: Uuid::new_v4(),
            generation: 1,
        };
        let foreign_effect = EffectRequest::FlushStaging {
            staging: staging_binding(
                foreign_registration,
                foreign_owner,
                ExpectedObject::Exact(test_object()),
            ),
            object: test_object(),
        };
        let stale = owner.submit(FileOperationCommand::for_test(
            OperationId::new(),
            ParticipantId::new(),
            EffectId::new(),
            vec![],
            foreign_registration,
            ExpectedObject::Exact(test_object()),
            foreign_effect,
            owner.test_mutating_authorization(vec![], 100, 0),
        ));
        let mut stale = stale.expect("foreign command reaches owner boundary");
        let stale_result = stale
            .recv_timeout(Duration::from_secs(1))
            .expect("foreign result");
        assert!(matches!(
            stale_result.outcome(),
            EffectOutcome::SupersededBeforeEffect { .. }
        ));
        stale.acknowledge().expect("foreign ack");
        let effect = unsupported_effect(&owner, registration, EffectId::new()).effect;
        let command = FileOperationCommand::for_test(
            OperationId::new(),
            ParticipantId::new(),
            EffectId::new(),
            vec![],
            registration,
            ExpectedObject::Exact(test_object()),
            effect.clone(),
            owner.test_observation_authorization(registration, vec![]),
        );
        assert!(matches!(
            owner.submit(command),
            Err(FileOperationOwnerQueueError::InvalidInput(_))
        ));
        owner.acknowledge_custody_for_test();
        let _ = owner.shutdown();
    }

    #[test]
    fn same_registration_can_bind_independent_source_and_destination_roles() {
        let owner = FileOperationOwner::start().expect("owner starts");
        let source_authority = AuthorityFence::Source(source_fence());
        let registration = owner
            .register_root_for_test(source_authority.clone(), "shared-source-root")
            .expect("registration");
        let destination = DestinationRoot {
            binding: RootRoleBinding {
                registration,
                authority: source_authority,
                locator: BoundedRelativeLocator::new("destination.wav").expect("locator"),
                expected: ExpectedObject::Absent,
            },
        };
        let effect = EffectRequest::MoveNoReplace {
            source: source_binding(registration, ExpectedObject::Exact(test_object())),
            destination,
            source_object: test_object(),
            expected_absent: AbsenceProof { marker: 1 },
        };
        let source_fences = vec![source_fence()];
        let mut command = command(
            registration,
            EffectId::new(),
            effect,
            owner.test_mutating_authorization(source_fences.clone(), 100, 0),
        );
        command.source_fences = source_fences.clone();
        if let EffectAuthorization::Mutating { live_intent, .. } = &mut command.authorization {
            live_intent.source_fences = source_fences;
        }
        let mut request = owner.submit(command).expect("same-root roles queue");
        let result = request
            .recv_timeout(Duration::from_secs(1))
            .expect("same-root result");
        assert!(matches!(
            result.outcome(),
            EffectOutcome::Unsupported { .. }
        ));
        assert_eq!(result.root_bindings().len(), 2);
        request.acknowledge().expect("same-root ack");
        owner.acknowledge_custody_for_test();
        assert_eq!(owner.shutdown(), FileOperationOwnerShutdown::DrainedAck);
    }

    #[test]
    fn effect_identity_predicates_are_checked_before_queue_admission() {
        let owner = FileOperationOwner::start().expect("owner starts");
        let registration = owner
            .register_root_for_test(profile_fence(owner.owner_identity()), "profile")
            .expect("registration");
        let staging = staging_binding(
            registration,
            owner.owner_identity(),
            ExpectedObject::Exact(test_object()),
        );
        let mismatched_flush = command(
            registration,
            EffectId::new(),
            EffectRequest::FlushStaging {
                staging: staging.clone(),
                object: OwnedObjectIdentity::new(),
            },
            owner.test_mutating_authorization(vec![], 100, 0),
        );
        assert!(matches!(
            owner.submit(mismatched_flush),
            Err(FileOperationOwnerQueueError::InvalidInput(_))
        ));
        let absent_source = SourceRoot {
            binding: RootRoleBinding {
                registration,
                authority: AuthorityFence::Source(source_fence()),
                locator: BoundedRelativeLocator::new("source.wav").expect("locator"),
                expected: ExpectedObject::Absent,
            },
        };
        let copy = command(
            registration,
            EffectId::new(),
            EffectRequest::CopyIntoStaging {
                source: absent_source,
                staging,
                source_range: BoundedRange::new(
                    BoundedOffset::new(0),
                    PositiveBoundedBytes::new(1).expect("range"),
                )
                .expect("range"),
                bytes: PositiveBoundedBytes::new(1).expect("bytes"),
            },
            owner.test_mutating_authorization(vec![source_fence()], 100, 0),
        );
        assert!(matches!(
            owner.submit(copy),
            Err(FileOperationOwnerQueueError::InvalidInput(_))
        ));

        let source_present = SourceRoot {
            binding: RootRoleBinding {
                registration,
                authority: AuthorityFence::Source(source_fence()),
                locator: BoundedRelativeLocator::new("source.wav").expect("locator"),
                expected: ExpectedObject::Exact(test_object()),
            },
        };
        let mut undersized_copy = command(
            registration,
            EffectId::new(),
            EffectRequest::CopyIntoStaging {
                source: source_present.clone(),
                staging: staging_binding(
                    registration,
                    owner.owner_identity(),
                    ExpectedObject::Exact(test_object()),
                ),
                source_range: BoundedRange::new(
                    BoundedOffset::new(0),
                    PositiveBoundedBytes::new(2).expect("range"),
                )
                .expect("range"),
                bytes: PositiveBoundedBytes::new(1).expect("undersized bytes"),
            },
            owner.test_mutating_authorization(vec![source_fence()], 100, 0),
        );
        undersized_copy.source_fences = vec![source_fence()];
        if let EffectAuthorization::Mutating { live_intent, .. } =
            &mut undersized_copy.authorization
        {
            live_intent.source_fences = undersized_copy.source_fences.clone();
        }
        assert!(matches!(
            owner.submit(undersized_copy),
            Err(FileOperationOwnerQueueError::InvalidInput(_))
        ));

        let mut larger_copy = command(
            registration,
            EffectId::new(),
            EffectRequest::CopyIntoStaging {
                source: source_present,
                staging: staging_binding(
                    registration,
                    owner.owner_identity(),
                    ExpectedObject::Exact(test_object()),
                ),
                source_range: BoundedRange::new(
                    BoundedOffset::new(0),
                    PositiveBoundedBytes::new(2).expect("range"),
                )
                .expect("range"),
                bytes: PositiveBoundedBytes::new(3).expect("larger bytes"),
            },
            owner.test_mutating_authorization(vec![source_fence()], 100, 0),
        );
        larger_copy.source_fences = vec![source_fence()];
        if let EffectAuthorization::Mutating { live_intent, .. } = &mut larger_copy.authorization {
            live_intent.source_fences = larger_copy.source_fences.clone();
        }
        let mut larger_request = owner.submit(larger_copy).expect("larger bound queues");
        let _ = larger_request
            .recv_timeout(Duration::from_secs(1))
            .expect("larger bound result");
        larger_request.acknowledge().expect("larger bound ack");
        owner.acknowledge_custody_for_test();
        assert_eq!(owner.shutdown(), FileOperationOwnerShutdown::DrainedAck);
    }

    #[test]
    fn drain_rejects_new_commands_and_shutdown_reports_unacknowledged_receipts() {
        let owner = FileOperationOwner::start().expect("owner starts");
        let registration = owner
            .register_root_for_test(profile_fence(owner.owner_identity()), "profile")
            .expect("registration");
        let request = owner.submit(unsupported_effect(&owner, registration, EffectId::new()));
        assert!(request.is_ok());
        owner.begin_drain().expect("drain");
        assert!(matches!(
            owner.submit(unsupported_effect(&owner, registration, EffectId::new())),
            Err(FileOperationOwnerQueueError::Draining)
        ));
        match owner.shutdown() {
            FileOperationOwnerShutdown::RetirementDeferred { pending, .. } => {
                assert_eq!(pending.len(), 1);
                assert_eq!(owner.status(), FileOperationOwnerStatus::Draining);
            }
            other => panic!("unexpected shutdown result: {other:?}"),
        }
    }

    #[test]
    fn unavailable_transition_cannot_be_overwritten_by_drain_or_finalization() {
        let owner = FileOperationOwner::start().expect("owner starts");
        let registration = owner
            .register_root_for_test(profile_fence(owner.owner_identity()), "profile")
            .expect("registration");
        let mut request = owner
            .submit(unsupported_effect(&owner, registration, EffectId::new()))
            .expect("command queues");
        let _ = request
            .recv_timeout(Duration::from_secs(1))
            .expect("result");
        request.acknowledge().expect("acknowledge result");

        owner.begin_drain().expect("begin drain");
        owner.shared.mark_unavailable("synthetic lifecycle race");
        assert!(matches!(
            owner.begin_drain(),
            Err(FileOperationOwnerQueueError::Unavailable(_))
        ));
        let shutdown = owner.shutdown();
        assert!(matches!(
            shutdown,
            FileOperationOwnerShutdown::RetirementUncertain { .. }
        ));
        assert!(matches!(
            owner.status(),
            FileOperationOwnerStatus::Unavailable { .. }
        ));
    }

    #[test]
    fn unavailable_transition_during_retained_outcome_publication_is_uncertain() {
        let owner = FileOperationOwner::start().expect("owner starts");
        let registration = owner
            .register_root_for_test(profile_fence(owner.owner_identity()), "profile")
            .expect("registration");
        let command = unsupported_effect(&owner, registration, EffectId::new());
        let identity = PendingEffectIdentity {
            operation_id: command.operation_id,
            participant_id: command.participant_id,
            effect_id: command.effect_id,
        };
        let mut request = owner.submit(command).expect("command queues");
        let _ = request
            .recv_timeout(Duration::from_secs(1))
            .expect("result");
        request.acknowledge().expect("acknowledge result");
        drop(request);

        owner.pause_before_retained_shutdown_outcome_for_test();
        let shared = Arc::clone(&owner.shared);
        let shutdown_thread = thread::spawn(move || owner.shutdown());
        let mut reached = shared
            .retained_shutdown_outcome_reached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while !*reached {
            reached = shared
                .retained_shutdown_outcome_cv
                .wait(reached)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }

        shared.mark_unavailable("synthetic outcome publication race");
        *shared
            .pause_before_retained_shutdown_outcome
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = false;
        shared.retained_shutdown_outcome_release_cv.notify_all();
        let shutdown = shutdown_thread.join().expect("shutdown thread");

        match shutdown {
            FileOperationOwnerShutdown::RetirementUncertain { pending, .. } => {
                assert!(pending.contains(&identity));
            }
            other => panic!("unexpected shutdown result: {other:?}"),
        }
        assert!(matches!(
            shared.status(),
            FileOperationOwnerStatus::Unavailable { .. }
        ));
        assert!(
            shared
                .retained_state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some()
        );
    }

    #[test]
    fn cancellation_only_accepts_known_effects_without_tombstones() {
        let owner = FileOperationOwner::start().expect("owner starts");
        let registration = owner
            .register_root_for_test(profile_fence(owner.owner_identity()), "profile")
            .expect("registration");
        let command = unsupported_effect(&owner, registration, EffectId::new());
        let identity = PendingEffectIdentity {
            operation_id: command.operation_id,
            participant_id: command.participant_id,
            effect_id: command.effect_id,
        };
        owner.pause_before_execute_for_test();
        let mut request = owner
            .submit(command.clone_for_test())
            .expect("command queues");
        assert_eq!(
            owner.cancel(identity),
            Ok(FileOperationCancelOutcome::KnownPending)
        );
        owner.release_worker_for_test();
        let _ = request
            .recv_timeout(Duration::from_secs(1))
            .expect("result");
        assert_eq!(owner.execution_count_for_test(), 0);
        request.acknowledge().expect("ack");
        drop(request);
        assert_eq!(
            owner.cancel(identity),
            Ok(FileOperationCancelOutcome::AlreadyCompleted)
        );
        let unknown = PendingEffectIdentity {
            operation_id: OperationId::new(),
            participant_id: ParticipantId::new(),
            effect_id: EffectId::new(),
        };
        assert_eq!(
            owner.cancel(unknown),
            Ok(FileOperationCancelOutcome::Unknown)
        );
        owner.acknowledge_custody_for_test();
        assert_eq!(owner.shutdown(), FileOperationOwnerShutdown::DrainedAck);
    }

    #[test]
    fn cancellation_after_execution_start_is_not_reported_as_completion() {
        let owner = FileOperationOwner::start().expect("owner starts");
        let registration = owner
            .register_root_for_test(profile_fence(owner.owner_identity()), "profile")
            .expect("registration");
        let command = unsupported_effect(&owner, registration, EffectId::new());
        let identity = PendingEffectIdentity {
            operation_id: command.operation_id,
            participant_id: command.participant_id,
            effect_id: command.effect_id,
        };
        owner.pause_after_execution_start_for_test();
        let mut request = owner
            .submit(command)
            .expect("command queues before execution barrier");
        owner.wait_until_execution_started_for_test();
        assert_eq!(
            owner.cancel(identity),
            Ok(FileOperationCancelOutcome::AlreadyStarted)
        );
        assert_eq!(owner.execution_count_for_test(), 0);
        owner.release_after_execution_start_for_test();
        let result = request
            .recv_timeout(Duration::from_secs(1))
            .expect("result after execution");
        assert!(matches!(
            result.outcome(),
            EffectOutcome::Unsupported { .. }
        ));
        assert_eq!(owner.execution_count_for_test(), 1);
        request.acknowledge().expect("acknowledge result");
        assert_eq!(
            owner.cancel(identity),
            Ok(FileOperationCancelOutcome::AlreadyCompleted)
        );
        owner.acknowledge_custody_for_test();
        assert_eq!(owner.shutdown(), FileOperationOwnerShutdown::DrainedAck);
    }

    #[test]
    fn command_queue_backpressure_is_deterministic_and_refunds_after_drain() {
        let owner = FileOperationOwner::start().expect("owner starts");
        let registration = owner
            .register_root_for_test(profile_fence(owner.owner_identity()), "profile")
            .expect("registration");
        owner.pause_before_execute_for_test();
        let first = owner
            .submit(unsupported_effect(&owner, registration, EffectId::new()))
            .expect("first command queues");
        owner.wait_until_worker_paused_for_test();
        let mut requests = Vec::new();
        requests.push(first);
        for _ in 0..FILE_OPERATION_OWNER_COMMAND_CAPACITY {
            requests.push(
                owner
                    .submit(unsupported_effect(&owner, registration, EffectId::new()))
                    .expect("bounded queue accepts first command plus its queue capacity"),
            );
        }
        assert!(matches!(
            owner.submit(unsupported_effect(&owner, registration, EffectId::new())),
            Err(FileOperationOwnerQueueError::Full)
        ));
        assert_eq!(
            owner.pending_effects().len(),
            FILE_OPERATION_OWNER_COMMAND_CAPACITY + 1
        );
        owner.release_worker_for_test();
        for mut request in requests {
            let result = request
                .recv_timeout(Duration::from_secs(1))
                .expect("queued result");
            assert!(matches!(
                result.outcome(),
                EffectOutcome::Unsupported { .. }
            ));
            acknowledge_with_bounded_retry(&mut request);
        }
        owner.acknowledge_custody_for_test();
        assert_eq!(owner.shutdown(), FileOperationOwnerShutdown::DrainedAck);
    }

    #[test]
    fn dropped_reply_and_acknowledged_tombstone_retain_full_evidence_until_custody() {
        let owner = FileOperationOwner::start().expect("owner starts");
        let registration = owner
            .register_root_for_test(profile_fence(owner.owner_identity()), "profile")
            .expect("registration");
        let dropped = unsupported_effect(&owner, registration, EffectId::new());
        let dropped_identity = PendingEffectIdentity {
            operation_id: dropped.operation_id,
            participant_id: dropped.participant_id,
            effect_id: dropped.effect_id,
        };
        let dropped_request = owner.submit(dropped).expect("dropped command queues");
        drop(dropped_request);

        let acknowledged = unsupported_effect(&owner, registration, EffectId::new());
        let acknowledged_identity = PendingEffectIdentity {
            operation_id: acknowledged.operation_id,
            participant_id: acknowledged.participant_id,
            effect_id: acknowledged.effect_id,
        };
        let mut acknowledged_request = owner.submit(acknowledged).expect("ack command queues");
        let acknowledged_result = acknowledged_request
            .recv_timeout(Duration::from_secs(1))
            .expect("ack result");
        acknowledged_request
            .acknowledge()
            .expect("acknowledge result");
        drop(acknowledged_request);

        let shutdown = owner.shutdown();
        assert!(matches!(
            shutdown,
            FileOperationOwnerShutdown::RetirementDeferred { .. }
        ));
        assert_eq!(owner.status(), FileOperationOwnerStatus::Draining);
        let evidence = owner.retained_evidence_for_test();
        assert!(evidence.iter().any(|(identity, bytes, fingerprint)| {
            *identity == dropped_identity
                && *bytes > 0
                && *fingerprint != RequestFingerprint([0; 32])
        }));
        assert!(evidence.iter().any(|(identity, bytes, fingerprint)| {
            *identity == acknowledged_identity
                && *bytes > 0
                && *fingerprint == acknowledged_result.fingerprint()
        }));
    }

    #[test]
    fn custody_ack_is_scoped_to_owner_and_exact_admitted_set() {
        let owner = FileOperationOwner::start().expect("owner starts");
        let registration = owner
            .register_root_for_test(profile_fence(owner.owner_identity()), "profile")
            .expect("registration");
        let mut request = owner
            .submit(unsupported_effect(&owner, registration, EffectId::new()))
            .expect("command queues");
        let _ = request
            .recv_timeout(Duration::from_secs(1))
            .expect("result");
        request.acknowledge().expect("ack");
        owner.acknowledge_custody_for_test_with_owner(OwnerIdentity(Uuid::new_v4()));
        assert!(matches!(
            owner.shutdown(),
            FileOperationOwnerShutdown::RetirementDeferred { .. }
        ));
        assert_eq!(owner.status(), FileOperationOwnerStatus::Draining);
    }

    #[test]
    fn custody_capture_while_queued_cannot_cover_a_later_result() {
        let owner = FileOperationOwner::start().expect("owner starts");
        let registration = owner
            .register_root_for_test(profile_fence(owner.owner_identity()), "profile")
            .expect("registration");
        owner.pause_before_execute_for_test();
        let mut request = owner
            .submit(unsupported_effect(&owner, registration, EffectId::new()))
            .expect("command queues");
        owner.acknowledge_custody_for_test();
        owner.release_worker_for_test();
        let _ = request
            .recv_timeout(Duration::from_secs(1))
            .expect("result");
        request.acknowledge().expect("ack");
        assert!(matches!(
            owner.shutdown(),
            FileOperationOwnerShutdown::RetirementDeferred { .. }
        ));
        assert_eq!(owner.status(), FileOperationOwnerStatus::Draining);
    }

    #[test]
    fn panic_retains_completed_and_queued_identities_without_clean_shutdown() {
        let owner = FileOperationOwner::start().expect("owner starts");
        let registration = owner
            .register_root_for_test(profile_fence(owner.owner_identity()), "profile")
            .expect("registration");
        owner.pause_before_execute_for_test();
        let first = unsupported_effect(&owner, registration, EffectId::new());
        let first_identity = PendingEffectIdentity {
            operation_id: first.operation_id,
            participant_id: first.participant_id,
            effect_id: first.effect_id,
        };
        let first_request = owner.submit(first).expect("first queues");
        owner.panic_for_test().expect("panic queues behind first");
        let second = unsupported_effect(&owner, registration, EffectId::new());
        let second_identity = PendingEffectIdentity {
            operation_id: second.operation_id,
            participant_id: second.participant_id,
            effect_id: second.effect_id,
        };
        let second_request = owner.submit(second).expect("second remains queued");
        owner.release_worker_for_test();
        drop(first_request);
        drop(second_request);
        let shutdown = owner.shutdown();
        assert!(matches!(
            shutdown,
            FileOperationOwnerShutdown::RetirementUncertain { .. }
        ));
        assert!(matches!(
            owner.status(),
            FileOperationOwnerStatus::Unavailable { .. }
        ));
        let pending = owner.pending_effects();
        assert!(pending.contains(&first_identity));
        assert!(pending.contains(&second_identity));
        let evidence = owner.retained_evidence_for_test();
        assert!(
            evidence
                .iter()
                .any(|(identity, bytes, _)| { *identity == first_identity && *bytes > 0 })
        );
        assert!(
            evidence
                .iter()
                .any(|(identity, bytes, _)| { *identity == second_identity && *bytes > 0 })
        );
    }

    #[test]
    fn result_permits_are_bounded_and_acknowledgement_refunds_the_handle() {
        let owner = FileOperationOwner::start().expect("owner starts");
        let registration = owner
            .register_root_for_test(profile_fence(owner.owner_identity()), "profile")
            .expect("registration");
        let mut requests = Vec::new();
        for _ in 0..FILE_OPERATION_OWNER_RESULT_CAPACITY {
            let command = unsupported_effect(&owner, registration, EffectId::new());
            loop {
                match owner.submit(command.clone_for_test()) {
                    Ok(request) => {
                        requests.push(request);
                        break;
                    }
                    Err(FileOperationOwnerQueueError::Full) => std::thread::yield_now(),
                    Err(error) => panic!("unexpected result-capacity setup error: {error:?}"),
                }
            }
        }
        let extra = owner.submit(unsupported_effect(&owner, registration, EffectId::new()));
        assert!(matches!(
            extra,
            Err(FileOperationOwnerQueueError::ResultsFull)
        ));
        for mut request in requests {
            let _ = request
                .recv_timeout(Duration::from_secs(1))
                .expect("result");
            loop {
                match request.acknowledge() {
                    Ok(()) => break,
                    Err(FileOperationRequestError::Acknowledgement(
                        FileOperationOwnerQueueError::Full,
                    )) => std::thread::yield_now(),
                    Err(error) => panic!("unexpected acknowledgement error: {error:?}"),
                }
            }
        }
        owner.acknowledge_custody_for_test();
        assert_eq!(owner.shutdown(), FileOperationOwnerShutdown::DrainedAck);
    }

    #[test]
    fn retained_input_bytes_are_bounded_and_refunded_after_acknowledgement() {
        let owner = FileOperationOwner::start().expect("owner starts");
        let registration = owner
            .register_root_for_test(profile_fence(owner.owner_identity()), "profile")
            .expect("registration");
        let staging = staging_binding(
            registration,
            owner.owner_identity(),
            ExpectedObject::Exact(test_object()),
        );
        let payload = BoundedBytes::new(vec![7; 240_000]).expect("bounded payload");
        let mut requests = Vec::new();
        let mut saw_input_backpressure = false;
        for _ in 0..16 {
            let effect = EffectRequest::WriteStagingChunk {
                staging: staging.clone(),
                chunk_id: ChunkId::new(),
                issue_id: IssueId::new(),
                output: OutputIdentity::new(),
                offset: BoundedOffset::new(0),
                length: PositiveBoundedBytes::new(240_000).expect("length"),
                payload: payload.clone(),
            };
            let command = command(
                registration,
                EffectId::new(),
                effect,
                owner.test_mutating_authorization(vec![], 240_000, 0),
            );
            loop {
                match owner.submit(command.clone_for_test()) {
                    Ok(request) => {
                        requests.push(request);
                        break;
                    }
                    Err(FileOperationOwnerQueueError::Full) => std::thread::yield_now(),
                    Err(FileOperationOwnerQueueError::InputBytesFull { .. }) => {
                        saw_input_backpressure = true;
                        break;
                    }
                    Err(error) => panic!("unexpected input-budget error: {error:?}"),
                }
            }
            if saw_input_backpressure {
                break;
            }
        }
        assert!(saw_input_backpressure);
        for mut request in requests {
            let _ = request
                .recv_timeout(Duration::from_secs(1))
                .expect("result");
            loop {
                match request.acknowledge() {
                    Ok(()) => break,
                    Err(FileOperationRequestError::Acknowledgement(
                        FileOperationOwnerQueueError::Full,
                    )) => std::thread::yield_now(),
                    Err(error) => panic!("unexpected input acknowledgement error: {error:?}"),
                }
            }
        }
        owner.acknowledge_custody_for_test();
        assert_eq!(owner.shutdown(), FileOperationOwnerShutdown::DrainedAck);
    }

    #[test]
    fn live_authorization_backing_is_charged_and_failed_send_refunds_exactly() {
        let owner = FileOperationOwner::start().expect("owner starts");
        let registration = owner
            .register_root_for_test(profile_fence(owner.owner_identity()), "profile")
            .expect("registration");
        owner.pause_before_execute_for_test();

        let mut accepted_command = unsupported_effect(&owner, registration, EffectId::new());
        let authorization_fences = vec![maximum_source_fence('a'), maximum_source_fence('b')];
        accepted_command.authorization =
            owner.test_mutating_authorization(authorization_fences, 100, 0);
        let canonical_len = accepted_command
            .canonical_bytes()
            .expect("authority-free canonical bytes")
            .len();
        let authorization_backing = accepted_command
            .authorization_backing_bytes()
            .expect("authorization backing bytes");
        let expected_charge = canonical_len
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_add(authorization_backing))
            .expect("bounded charge");
        let before = owner.input_bytes_remaining_for_test();
        let accepted = owner
            .submit(accepted_command)
            .expect("mismatched but bounded authorization queues");
        owner.wait_until_worker_paused_for_test();
        assert_eq!(
            before - owner.input_bytes_remaining_for_test(),
            expected_charge,
            "authorization backing must remain covered after canonicalization",
        );

        let mut requests = vec![accepted];
        for _ in 0..FILE_OPERATION_OWNER_COMMAND_CAPACITY {
            requests.push(
                owner
                    .submit(unsupported_effect(&owner, registration, EffectId::new()))
                    .expect("bounded queue accepts first command plus its queue capacity"),
            );
        }
        let failed_before = owner.input_bytes_remaining_for_test();
        let mut failed_command = unsupported_effect(&owner, registration, EffectId::new());
        failed_command.authorization = owner.test_mutating_authorization(
            vec![maximum_source_fence('c'), maximum_source_fence('d')],
            100,
            0,
        );
        assert!(matches!(
            owner.submit(failed_command),
            Err(FileOperationOwnerQueueError::Full)
        ));
        assert_eq!(
            owner.input_bytes_remaining_for_test(),
            failed_before,
            "a failed queue send must refund its complete authorization charge",
        );

        owner.release_worker_for_test();
        for mut request in requests {
            let _ = request
                .recv_timeout(Duration::from_secs(1))
                .expect("queued result");
            acknowledge_with_bounded_retry(&mut request);
        }
        assert!(owner.input_bytes_remaining_for_test() < FILE_OPERATION_OWNER_MAX_INPUT_BYTES);
        owner.acknowledge_custody_for_test();
        assert_eq!(owner.shutdown(), FileOperationOwnerShutdown::DrainedAck);
        assert_eq!(
            owner.input_bytes_remaining_for_test(),
            FILE_OPERATION_OWNER_MAX_INPUT_BYTES
        );
    }

    #[test]
    fn panic_is_sticky_and_retains_no_clean_shutdown_claim() {
        let owner = FileOperationOwner::start().expect("owner starts");
        owner.panic_for_test().expect("panic command queues");
        let shutdown = owner.shutdown();
        assert!(matches!(
            owner.status(),
            FileOperationOwnerStatus::Unavailable { .. }
        ));
        assert!(matches!(
            owner.submit(unsupported_effect(
                &owner,
                RootRegistrationRef {
                    owner: owner.owner_identity(),
                    registration_id: Uuid::new_v4(),
                    generation: 1,
                },
                EffectId::new(),
            )),
            Err(FileOperationOwnerQueueError::Unavailable(_))
        ));
        assert!(matches!(
            shutdown,
            FileOperationOwnerShutdown::RetirementUncertain { .. }
        ));
    }

    #[test]
    fn bounded_locators_epochs_and_checked_ranges_fail_closed() {
        assert!(BoundedRelativeLocator::new("../escape").is_err());
        assert!(BoundedRelativeLocator::new("./escape").is_err());
        assert!(BoundedRelativeLocator::new("escape/./nested").is_err());
        assert!(BoundedRelativeLocator::new("escape//nested").is_err());
        assert!(
            SourceFence::new(SourceId::from_string("source"), "root", "db", 0, None, 0).is_err()
        );
        let max_generation = RootRegistrationRef {
            owner: OwnerIdentity(Uuid::new_v4()),
            registration_id: Uuid::new_v4(),
            generation: u64::MAX,
        };
        assert!(max_generation.next_generation().is_err());
        assert!(
            SourceFence::new(
                SourceId::from_string("source"),
                "root",
                "db",
                u64::MAX,
                Some(u64::MAX),
                u64::MAX,
            )
            .is_ok()
        );
        let length = PositiveBoundedBytes::new(1).expect("length");
        assert!(BoundedRange::new(BoundedOffset::new(u64::MAX), length).is_err());

        let owner = FileOperationOwner::start().expect("owner starts");
        let registration = owner
            .register_root_with_generation_for_test(
                profile_fence(owner.owner_identity()),
                "maximum-generation",
                u64::MAX,
            )
            .expect("maximum generation registration");
        let mut request = owner
            .submit(unsupported_effect(&owner, registration, EffectId::new()))
            .expect("maximum generation command queues");
        let result = request
            .recv_timeout(Duration::from_secs(1))
            .expect("maximum generation result");
        assert!(matches!(
            result.outcome(),
            EffectOutcome::Unsupported { .. }
        ));
        request.acknowledge().expect("maximum generation ack");
        let max_source = SourceFence::new(
            SourceId::from_string("source"),
            "root",
            "db",
            u64::MAX,
            Some(u64::MAX),
            u64::MAX,
        )
        .expect("maximum source fence");
        let source_registration = owner
            .register_root_with_generation_for_test(
                AuthorityFence::Source(max_source.clone()),
                "maximum-source",
                u64::MAX,
            )
            .expect("maximum source registration");
        let destination = DestinationRoot {
            binding: RootRoleBinding {
                registration: source_registration,
                authority: AuthorityFence::Source(max_source.clone()),
                locator: BoundedRelativeLocator::new("max-destination").expect("locator"),
                expected: ExpectedObject::Absent,
            },
        };
        let source = SourceRoot {
            binding: RootRoleBinding {
                registration: source_registration,
                authority: AuthorityFence::Source(max_source.clone()),
                locator: BoundedRelativeLocator::new("max-source").expect("locator"),
                expected: ExpectedObject::Exact(test_object()),
            },
        };
        let mut max_source_command = command(
            source_registration,
            EffectId::new(),
            EffectRequest::MoveNoReplace {
                source,
                destination,
                source_object: test_object(),
                expected_absent: AbsenceProof { marker: 2 },
            },
            owner.test_mutating_authorization(vec![max_source.clone()], 100, 0),
        );
        max_source_command.source_fences = vec![max_source.clone()];
        if let EffectAuthorization::Mutating { live_intent, .. } =
            &mut max_source_command.authorization
        {
            live_intent.source_fences = vec![max_source];
        }
        let mut max_source_request = owner
            .submit(max_source_command)
            .expect("maximum source command queues");
        let max_source_result = max_source_request
            .recv_timeout(Duration::from_secs(1))
            .expect("maximum source result");
        assert!(matches!(
            max_source_result.outcome(),
            EffectOutcome::Unsupported { .. }
        ));
        max_source_request
            .acknowledge()
            .expect("maximum source ack");
        owner.acknowledge_custody_for_test();
        assert_eq!(owner.shutdown(), FileOperationOwnerShutdown::DrainedAck);
    }

    // This helper keeps the conflict test's registration identity construction inside the
    // owner module's fixture-only boundary rather than exposing a production constructor.
    trait TestRegistrationLookup {
        fn owner_identity_ref_for_test(
            &self,
            registration: RootRegistrationRef,
        ) -> RootRegistrationRef;
    }

    impl TestRegistrationLookup for FileOperationOwner {
        fn owner_identity_ref_for_test(
            &self,
            registration: RootRegistrationRef,
        ) -> RootRegistrationRef {
            registration
        }
    }
}
