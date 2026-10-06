//! Bounded owner observations and metadata-only lifecycle proposals (OPT-2192).
//! No value in this module is a registration, capacity permission or completion grant.
use super::{
    SourceAuthorityRegistration, SourceDatabase, SourceDbError, SourceDirectoryTruthState,
    SourceScanFence, SourceWriterError,
};
use crate::operation_journal::{
    OperationId,
    work_plan::{WorkUnitKey, WorkUnitRole},
};
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use thiserror::Error;
use uuid::Uuid;

/// Complete canonical proposal ceiling, including JSON escaping and all identities.
pub const SOURCE_REGISTRY_REFERENCE_MAX_BYTES: usize = 16 * 1024;
const IDENTITY_BYTES: usize = 1024;
const ROOT_BYTES: usize = 4096;

/// Observed source-owner phase, never proof of complete journal/reserve installation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceRegistryRegistrationPhase {
    /// Source-owner preparation exists; journal installation is not confirmed.
    Prepared,
    /// Source state observed REGISTERED; journal readiness/capacity is still unproved.
    RegisteredPhaseObserved,
    /// Source admission is draining.
    Draining,
    /// Source state reports retirement.
    Retired,
    /// Source preparation was abandoned.
    Abandoned,
}

/// Scalar registration observation. It cannot create the opaque registration receipt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRegistryRegistrationObservation {
    pub(crate) registration_id: Uuid,
    pub(crate) phase: SourceRegistryRegistrationPhase,
}
impl SourceRegistryRegistrationObservation {
    /// Historical registration identifier, not authentication.
    pub fn registration_id(&self) -> Uuid {
        self.registration_id
    }
    /// The sampled source state does not establish journal registration readiness.
    pub fn phase(&self) -> SourceRegistryRegistrationPhase {
        self.phase
    }
}

/// Optional completed-audit metadata at the captured revision; no fabricated pre-audit values.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceRegistryAuditObservation {
    /// Completed audit metadata was unavailable or not current.
    Unavailable {},
    /// Current bounded completed-audit observations.
    Captured {
        /// Exact observed completed revision.
        revision: u64,
        /// Active directory generation at that revision.
        directory_generation: u64,
    },
}

mod audit_decoder;
mod expected_decoder;

/// Historical scalar subject. Deserialization confers no source custody or freshness.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRegistryReferenceSubject {
    source_id: String,
    lifecycle_generation: u64,
    profile_acquisition: Uuid,
    lease_owner: Uuid,
    lease_epoch: u64,
    media_root_identity: Vec<u8>,
    database_root_identity: String,
    database_identity: String,
    root_binding_identity: String,
    scan_revision: u64,
    audit: SourceRegistryAuditObservation,
    registration: Option<SourceRegistryRegistrationObservation>,
}
impl SourceRegistryReferenceSubject {
    // Complete separately allocated scalar payload; inline headers are charged by its owner.
    pub(crate) fn retained_heap_bytes(&self) -> Option<usize> {
        [
            self.source_id.capacity(),
            self.media_root_identity.capacity(),
            self.database_root_identity.capacity(),
            self.database_identity.capacity(),
            self.root_binding_identity.capacity(),
        ]
        .into_iter()
        .try_fold(0usize, usize::checked_add)
    }
    /// Captured source identity.
    pub fn source_id(&self) -> &str {
        &self.source_id
    }
    /// Captured lifecycle, not a monotonic profile epoch.
    pub fn lifecycle_generation(&self) -> u64 {
        self.lifecycle_generation
    }
    /// Historical profile acquisition equality fence.
    pub fn profile_acquisition(&self) -> Uuid {
        self.profile_acquisition
    }
    /// Historical source lease owner equality fence.
    pub fn lease_owner(&self) -> Uuid {
        self.lease_owner
    }
    /// Captured source epoch.
    pub fn lease_epoch(&self) -> u64 {
        self.lease_epoch
    }
    /// Descriptor-derived media root bytes, never a pathname.
    pub fn media_root_identity(&self) -> &[u8] {
        &self.media_root_identity
    }
    /// Descriptor-derived database root identity.
    pub fn database_root_identity(&self) -> &str {
        &self.database_root_identity
    }
    /// Retained main-file identity.
    pub fn database_identity(&self) -> &str {
        &self.database_identity
    }
    /// Retained file's root binding.
    pub fn root_binding_identity(&self) -> &str {
        &self.root_binding_identity
    }
    /// Observed scan revision, possibly pre-audit.
    pub fn scan_revision(&self) -> u64 {
        self.scan_revision
    }
    /// Completed-audit observation or explicit unavailability.
    pub fn audit(&self) -> &SourceRegistryAuditObservation {
        &self.audit
    }
    /// Absence explicitly means no registration observation, not enrollment.
    pub fn registration(&self) -> Option<&SourceRegistryRegistrationObservation> {
        self.registration.as_ref()
    }
}

/// Captured only by a real retained writer. Not deserializable and never a live permit.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SourceRegistryReferenceObservation {
    subject: SourceRegistryReferenceSubject,
}
impl SourceRegistryReferenceObservation {
    /// Borrow historical metadata. No conversion to SourceRevision or registration authority.
    pub fn subject(&self) -> &SourceRegistryReferenceSubject {
        &self.subject
    }
}

/// One proposed global metadata transition; execution requires separate authenticated owners.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceRegistryReferenceTransition {
    /// Proposed enrollment, not an applied result.
    Enrolled,
    /// Proposed restrictive drain state.
    Draining,
    /// Proposed retirement, requiring separate sealed close evidence at execution.
    Retired,
}

/// Exact expected prior relation identity; a hash is observation, never proof of a row.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceRegistryReferenceExpected {
    /// No prior relation is expected.
    Absent {},
    /// Exact previous metadata relation is expected.
    Present {
        /// Previous finite relation state.
        state: SourceRegistryReferenceTransition,
        /// Identity of the complete expected prior row.
        row_identity: [u8; 32],
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredProposal {
    version: u8,
    operation_id: OperationId,
    work_key: WorkUnitKey,
    transition: SourceRegistryReferenceTransition,
    expected: SourceRegistryReferenceExpected,
    subject: SourceRegistryReferenceSubject,
}

/// Canonical metadata only. Decoding never recreates an owner-issued observation or authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceRegistryReferenceProposal {
    stored: StoredProposal,
    canonical: Vec<u8>,
    identity: [u8; 32],
}
impl SourceRegistryReferenceProposal {
    /// Form one bounded proposal from a genuine capture. A registration phase is not permission.
    pub fn new(
        observation: &SourceRegistryReferenceObservation,
        operation_id: OperationId,
        work_key: WorkUnitKey,
        transition: SourceRegistryReferenceTransition,
        expected: SourceRegistryReferenceExpected,
    ) -> Result<Self, SourceRegistryReferenceError> {
        Self::from_stored(StoredProposal {
            version: 1,
            operation_id,
            work_key,
            transition,
            expected,
            subject: observation.subject.clone(),
        })
    }
    /// Strict bounded metadata decode: duplicates, unknown fields/versions and malformed values reject.
    pub fn decode(bytes: &[u8]) -> Result<Self, SourceRegistryReferenceError> {
        if bytes.len() > SOURCE_REGISTRY_REFERENCE_MAX_BYTES {
            return Err(SourceRegistryReferenceError::TooLarge);
        }
        Self::from_stored(serde_json::from_slice(bytes)?)
    }
    fn from_stored(stored: StoredProposal) -> Result<Self, SourceRegistryReferenceError> {
        validate_proposal(&stored)?;
        let canonical = bounded_json(&stored)?;
        let mut hash = blake3::Hasher::new();
        hash.update(b"yield.source-registry-reference.proposal.v1\0");
        hash.update(&canonical);
        let identity = *hash.finalize().as_bytes();
        Ok(Self {
            stored,
            canonical,
            identity,
        })
    }
    /// Immutable operation correlation.
    pub fn operation_id(&self) -> OperationId {
        self.stored.operation_id
    }
    /// Full frozen participant/work role/key correlation.
    pub fn work_key(&self) -> WorkUnitKey {
        self.stored.work_key
    }
    /// Requested metadata state, not an applied disposition.
    pub fn transition(&self) -> SourceRegistryReferenceTransition {
        self.stored.transition
    }
    /// Expected historical row identity.
    pub fn expected(&self) -> &SourceRegistryReferenceExpected {
        &self.stored.expected
    }
    /// Historical metadata; cannot mint the non-deserializable capture wrapper.
    pub fn subject(&self) -> &SourceRegistryReferenceSubject {
        &self.stored.subject
    }
    /// Complete canonical bytes, bounded to 16 KiB.
    pub fn canonical_bytes(&self) -> &[u8] {
        &self.canonical
    }
    /// Domain-separated identity over every semantic field, including operation and full work key.
    pub fn payload_identity(&self) -> &[u8; 32] {
        &self.identity
    }
}

/// Read/capture/codec failures grant no effect or successful lifecycle outcome.
#[derive(Debug, Error)]
pub enum SourceRegistryReferenceError {
    /// Actual retained writer refused the observation.
    #[error(transparent)]
    Writer(#[from] SourceWriterError),
    /// Actual owned database failed a bounded read or ownership validation.
    #[error(transparent)]
    Database(#[from] SourceDbError),
    /// Canonical encoding or input exceeds its fixed limit.
    #[error("source registry reference encoding exceeds 16 KiB")]
    TooLarge,
    /// Unknown codec versions cannot be interpreted as current proposals.
    #[error("unsupported source registry reference version")]
    UnsupportedVersion,
    /// Registration absence cannot become enrollment or any reference transition.
    #[error("source registration observation unavailable")]
    RegistrationUnavailable,
    /// Supplied guard belongs to another retained source.
    #[error("source registration observation belongs to a different owner")]
    RegistrationMismatch,
    /// A phase/revision changed while capture was in progress.
    #[error("source registry observation changed during capture")]
    ObservationChanged,
    /// Invalid or out-of-policy scalar field.
    #[error("invalid source registry reference field: {0}")]
    Invalid(&'static str),
    /// Strict serialization/parse failure.
    #[error(transparent)]
    Codec(#[from] serde_json::Error),
}

fn uuid(text: &str) -> Result<Uuid, SourceRegistryReferenceError> {
    let id =
        Uuid::parse_str(text).map_err(|_| SourceRegistryReferenceError::Invalid("owner UUID"))?;
    if id.is_nil() || id.to_string() != text {
        return Err(SourceRegistryReferenceError::Invalid("owner UUID"));
    }
    Ok(id)
}
fn text(value: &str, limit: usize) -> bool {
    !value.trim().is_empty() && value.len() <= limit && !value.chars().any(char::is_control)
}
fn integer(value: u64, positive: bool) -> bool {
    value <= i64::MAX as u64 && (!positive || value > 0)
}
pub(crate) fn validate_subject(
    s: &SourceRegistryReferenceSubject,
) -> Result<(), SourceRegistryReferenceError> {
    if !text(&s.source_id, 256)
        || !integer(s.lifecycle_generation, true)
        || !integer(s.lease_epoch, true)
        || !integer(s.scan_revision, false)
        || s.profile_acquisition.is_nil()
        || s.lease_owner.is_nil()
    {
        return Err(SourceRegistryReferenceError::Invalid(
            "source/owner/generation",
        ));
    }
    if s.media_root_identity.is_empty()
        || s.media_root_identity.len() > ROOT_BYTES
        || !text(&s.database_root_identity, IDENTITY_BYTES)
        || !text(&s.database_identity, IDENTITY_BYTES)
        || !text(&s.root_binding_identity, IDENTITY_BYTES)
    {
        return Err(SourceRegistryReferenceError::Invalid(
            "root/database identity",
        ));
    }
    if let Some(r) = &s.registration {
        if r.registration_id.is_nil() {
            return Err(SourceRegistryReferenceError::Invalid("registration UUID"));
        }
    }
    if let SourceRegistryAuditObservation::Captured {
        revision,
        directory_generation,
    } = s.audit
    {
        if revision != s.scan_revision
            || !integer(revision, false)
            || !integer(directory_generation, true)
        {
            return Err(SourceRegistryReferenceError::Invalid("audit"));
        }
    }
    Ok(())
}
fn validate_proposal(s: &StoredProposal) -> Result<(), SourceRegistryReferenceError> {
    if s.version != 1 {
        return Err(SourceRegistryReferenceError::UnsupportedVersion);
    }
    validate_subject(&s.subject)?;
    let registration = s
        .subject
        .registration
        .as_ref()
        .ok_or(SourceRegistryReferenceError::RegistrationUnavailable)?;
    if registration.phase != SourceRegistryRegistrationPhase::RegisteredPhaseObserved
        && registration.phase != SourceRegistryRegistrationPhase::Draining
    {
        return Err(SourceRegistryReferenceError::RegistrationUnavailable);
    }
    if s.transition == SourceRegistryReferenceTransition::Enrolled
        && registration.phase != SourceRegistryRegistrationPhase::RegisteredPhaseObserved
    {
        return Err(SourceRegistryReferenceError::Invalid("enrollment phase"));
    }
    if s.operation_id.as_uuid().is_nil()
        || s.work_key.participant_id().as_uuid().is_nil()
        || s.work_key.role() != WorkUnitRole::GlobalLibrary
    {
        return Err(SourceRegistryReferenceError::Invalid("operation/work key"));
    }
    match (&s.expected, s.transition) {
        (
            SourceRegistryReferenceExpected::Absent {},
            SourceRegistryReferenceTransition::Enrolled,
        ) => {}
        (
            SourceRegistryReferenceExpected::Present {
                state: SourceRegistryReferenceTransition::Enrolled,
                ..
            },
            SourceRegistryReferenceTransition::Draining,
        ) => {}
        (
            SourceRegistryReferenceExpected::Present {
                state: SourceRegistryReferenceTransition::Draining,
                ..
            },
            SourceRegistryReferenceTransition::Retired,
        ) => {}
        _ => return Err(SourceRegistryReferenceError::Invalid("state transition")),
    }
    Ok(())
}
struct BoundedOutput {
    bytes: Vec<u8>,
    full: bool,
}
impl Write for BoundedOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self
            .bytes
            .len()
            .checked_add(bytes.len())
            .is_none_or(|n| n > SOURCE_REGISTRY_REFERENCE_MAX_BYTES)
        {
            self.full = true;
            return Err(io::Error::other("bounded proposal"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn bounded_json<T: Serialize>(value: &T) -> Result<Vec<u8>, SourceRegistryReferenceError> {
    let mut output = BoundedOutput {
        bytes: Vec::new(),
        full: false,
    };
    if let Err(error) = serde_json::to_writer(&mut output, value) {
        return Err(if output.full {
            SourceRegistryReferenceError::TooLarge
        } else {
            SourceRegistryReferenceError::Codec(error)
        });
    }
    Ok(output.bytes)
}

pub(crate) fn capture(
    database: &SourceDatabase,
    fence: &SourceScanFence,
    registration: Option<&SourceAuthorityRegistration>,
) -> Result<SourceRegistryReferenceObservation, SourceRegistryReferenceError> {
    capture_impl(database, fence, registration, false)
}

pub(crate) fn capture_in_transaction(
    database: &SourceDatabase,
    fence: &SourceScanFence,
) -> Result<SourceRegistryReferenceObservation, SourceRegistryReferenceError> {
    if database.connection.is_autocommit() {
        return Err(SourceDbError::Unexpected.into());
    }
    capture_impl(database, fence, None, true)
}

pub(crate) fn capture_registered_in_transaction(
    database: &SourceDatabase,
    fence: &SourceScanFence,
    registration: &SourceAuthorityRegistration,
) -> Result<SourceRegistryReferenceObservation, SourceRegistryReferenceError> {
    if database.connection.is_autocommit() {
        return Err(SourceDbError::Unexpected.into());
    }
    capture_impl(database, fence, Some(registration), true)
}

fn capture_impl(
    database: &SourceDatabase,
    fence: &SourceScanFence,
    registration: Option<&SourceAuthorityRegistration>,
    in_transaction: bool,
) -> Result<SourceRegistryReferenceObservation, SourceRegistryReferenceError> {
    let profile = database
        .profile_guard
        .as_ref()
        .ok_or(SourceDbError::ProfileOwnershipRequired)?;
    database.validate_profile_owned_open(profile)?;
    let lease = database
        .source_lease
        .as_ref()
        .ok_or(SourceDbError::ProfileOwnershipRequired)?;
    let binding = database
        .database_file_binding
        .as_ref()
        .ok_or(SourceDbError::ProfileOwnershipRequired)?;
    let registration = registration
        .map(|r| r.registry_reference_observation(database))
        .transpose()?;
    let audit = if in_transaction {
        database.last_authoritative_audit_metadata_in_transaction()?
    } else {
        database.last_authoritative_audit_metadata()?
    };
    // Existing BoundedPage inspection, not FullIntegrity's table traversal.
    let directory_state = if in_transaction {
        database.source_directory_truth_state_bounded_in_transaction()?
    } else {
        database.source_directory_truth_state_bounded()?
    };
    let audit = match (audit, directory_state) {
        (
            Some(a),
            SourceDirectoryTruthState::Active {
                generation,
                published_source_revision,
            },
        ) if a.source_revision == fence.revision()
            && published_source_revision == fence.revision()
            && a.root_identity == *fence.root_identity() =>
        {
            SourceRegistryAuditObservation::Captured {
                revision: fence.revision(),
                directory_generation: generation,
            }
        }
        _ => SourceRegistryAuditObservation::Unavailable {},
    };
    let subject = SourceRegistryReferenceSubject {
        source_id: fence.source_id().as_str().into(),
        lifecycle_generation: fence.lifecycle_generation(),
        profile_acquisition: uuid(profile.owner_identity())?,
        lease_owner: uuid(lease.owner_identity())?,
        lease_epoch: fence.lease_epoch(),
        media_root_identity: fence.root_identity().as_bytes().into(),
        database_root_identity: lease.database_root_binding().identity().into(),
        database_identity: binding.identity().into(),
        root_binding_identity: binding.root().identity().into(),
        scan_revision: fence.revision(),
        audit,
        registration,
    };
    validate_subject(&subject)?;
    let observation = SourceRegistryReferenceObservation { subject };
    bounded_json(&observation)?;
    database.validate_profile_owned_open(profile)?;
    Ok(observation)
}

#[cfg(test)]
mod tests;
