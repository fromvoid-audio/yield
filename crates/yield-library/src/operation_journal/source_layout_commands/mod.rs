//! Owner-derived layout intent data. Historical bytes never carry admission or live ownership.
use super::ParticipantId;
use crate::sample_sources::db::write::{SourceLayoutGeneration, SourcePreparedLayoutGeneration};
use crate::sample_sources::db::{SourceCommandId, SourceLayoutMetadataBudget, SourceWriter};
use std::{
    fmt,
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};
use thiserror::Error;
use uuid::Uuid;
mod codec;
pub use codec::decode_source_layout_intent;

/// Version of this passive input envelope, distinct from the journal's record/family version.
pub const SOURCE_LAYOUT_INTENT_VERSION: u64 = 1;
/// Binary request cap: one <=512KiB generation plus a bounded <=8KiB header.
pub const MAX_SOURCE_LAYOUT_INTENT_BYTES: usize = 512 * 1024 + 8 * 1024;

/// Immutable passive source-only layout input. No public scalar or historical constructor.
#[derive(Clone, Eq, PartialEq)]
pub struct SourceLayoutIntentArgs {
    participant: ParticipantId,
    command: SourceCommandId,
    scope: SourceLayoutHistoricalScope,
    allocation: SourceLayoutAllocationObservation,
    generation: Vec<u8>,
}
impl fmt::Debug for SourceLayoutIntentArgs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceLayoutIntentArgs")
            .field("generation_bytes", &self.generation.len())
            .finish_non_exhaustive()
    }
}
/// Bounded source-data validation errors; no operation or retry grant is returned.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum SourceLayoutIntentError {
    /// Original retained source, prepared input or allocation observations no longer validate.
    #[error("source layout observation unavailable or stale")]
    Observation,
    /// Original cooperative allocation deadline has expired.
    #[error("source layout observation expired")]
    Deadline,
    /// Preparation was cancelled without allocating durable identity.
    #[error("source layout input cancelled")]
    Cancelled,
    /// Historical identities or generation scope do not agree.
    #[error("invalid source layout intent identity")]
    Identity,
    /// The finite source allocation arithmetic or policy facts are inconsistent.
    #[error("invalid source layout allocation observation")]
    Allocation,
    /// Generation is not complete canonical bounded historical data.
    #[error("invalid canonical source layout generation")]
    Generation,
    /// The binary input or a length exceeds its finite bound.
    #[error("source layout intent byte bound")]
    Bytes,
    /// Supported wire data is corrupt, truncated, ambiguous or noncanonical.
    #[error("invalid source layout intent wire data")]
    Wire,
}
impl SourceLayoutIntentArgs {
    /// Build passive data from both genuine original source observations, never admission.
    pub fn from_prepared(
        writer: &SourceWriter,
        prepared: &SourcePreparedLayoutGeneration,
        budget: &SourceLayoutMetadataBudget,
        participant: ParticipantId,
        command: SourceCommandId,
        cancel: &AtomicBool,
    ) -> Result<Self, SourceLayoutIntentError> {
        use SourceLayoutIntentError as E;
        let deadline = budget.observation_deadline();
        let check = || {
            if cancel.load(Ordering::Acquire) {
                return Err(E::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(E::Deadline);
            }
            if prepared.revision() != budget.revision() {
                return Err(E::Observation);
            }
            writer
                .validate_layout_metadata_budget(budget)
                .map_err(|_| E::Observation)?;
            writer
                .validate_prepared_layout_generation(prepared, cancel, deadline)
                .map_err(|_| E::Observation)?;
            if Instant::now() >= deadline {
                return Err(E::Deadline);
            }
            if cancel.load(Ordering::Acquire) {
                return Err(E::Cancelled);
            }
            Ok(())
        };
        check()?;
        let reference = writer.reference_read_fence().map_err(|_| E::Observation)?;
        if reference.source_revision() != budget.revision() {
            return Err(E::Observation);
        }
        let profile = reference
            .profile_acquisition_identity()
            .ok_or(E::Observation)?;
        let profile = Uuid::parse_str(&profile).map_err(|_| E::Identity)?;
        let revision = budget.revision();
        if revision.source_id().as_str().len() > 256
            || revision.root_identity().as_bytes().len() > 4096
            || revision.root_binding_identity().len() > 512
            || revision.database_identity().len() > 512
            || prepared.bytes().len() > 512 * 1024
        {
            return Err(E::Bytes);
        }
        let value = Self {
            participant,
            command,
            scope: SourceLayoutHistoricalScope {
                profile,
                source: revision.source_id().as_str().to_owned(),
                root: revision.root_identity().as_bytes().to_vec(),
                root_binding: revision.root_binding_identity().to_owned(),
                database: revision.database_identity().to_owned(),
                lifecycle: revision.lifecycle_generation(),
                lease: revision.lease_epoch(),
                revision: revision.revision(),
            },
            allocation: SourceLayoutAllocationObservation {
                page_size: budget.page_size(),
                page_count: budget.page_count(),
                database_bytes: budget.resident_database_bytes(),
                maximum_pages: budget.maximum_pages(),
                wal_bytes: budget.resident_wal_bytes(),
                shm_bytes: budget.resident_shm_bytes(),
                prior_bytes: budget.existing_generation_bytes(),
                prior_digest: budget.existing_generation_digest(),
                peak: budget.required_metadata_bytes(),
            },
            generation: prepared.bytes().to_vec(),
        };
        value.validate()?;
        reference.validate().map_err(|_| E::Observation)?;
        check()?;
        Ok(value)
    }
    /// Historical source/profile context, never a conversion into a fresh source revision.
    pub fn scope(&self) -> &SourceLayoutHistoricalScope {
        &self.scope
    }
    /// Exact source participant correlation, not an owner-issued completion receipt.
    pub fn participant_id(&self) -> ParticipantId {
        self.participant
    }
    /// Exact source command correlation, never an allocated OperationId.
    pub fn source_command_id(&self) -> SourceCommandId {
        self.command
    }
    /// Original measured source allocation data, never a capacity claim.
    pub fn allocation(&self) -> &SourceLayoutAllocationObservation {
        &self.allocation
    }
    /// Exact complete generation bytes; they cannot authorize SQL publication.
    pub fn generation_bytes(&self) -> &[u8] {
        &self.generation
    }
    /// Encode strict bounded canonical binary input; no journal record is persisted.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, SourceLayoutIntentError> {
        codec::encode(self)
    }
    /// Validate historical shape/identity/arithmetic only; current admission requires real owners.
    pub fn validate(&self) -> Result<(), SourceLayoutIntentError> {
        use SourceLayoutIntentError as E;
        let r = &self.scope;
        let text =
            |s: &str, max: usize| !s.trim().is_empty() && s.len() <= max && !s.contains('\0');
        if self.participant.as_uuid().is_nil()
            || self.command.as_uuid().is_nil()
            || r.profile.is_nil()
            || !text(&r.source, 256)
            || r.root.is_empty()
            || r.root.len() > 4096
            || !text(&r.root_binding, 512)
            || !text(&r.database, 512)
            || [r.lifecycle, r.lease, r.revision]
                .iter()
                .any(|&n| n == 0 || n > i64::MAX as u64)
        {
            return Err(E::Identity);
        }
        self.allocation.validate()?;
        let generation =
            SourceLayoutGeneration::decode(&self.generation).map_err(|_| E::Generation)?;
        if generation.source_id() != r.source
            || generation.root_identity() != r.root
            || generation.root_binding_identity() != r.root_binding
            || generation.database_identity() != r.database
            || generation.origin_revision() != r.revision
        {
            return Err(E::Identity);
        }
        Ok(())
    }
}

/// Historical correlation only. No serialized source/root/profile data restores a capability.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceLayoutHistoricalScope {
    profile: Uuid,
    source: String,
    root: Vec<u8>,
    root_binding: String,
    database: String,
    lifecycle: u64,
    lease: u64,
    revision: u64,
}
impl SourceLayoutHistoricalScope {
    /// Historical profile acquisition UUID, never a writable profile guard.
    pub fn profile_acquisition_identity(&self) -> Uuid {
        self.profile
    }
    /// Historical logical source identity.
    pub fn source_id(&self) -> &str {
        &self.source
    }
    /// Exact historical audited media-root bytes.
    pub fn root_identity(&self) -> &[u8] {
        &self.root
    }
    /// Historical retained source-database directory identity.
    pub fn root_binding_identity(&self) -> &str {
        &self.root_binding
    }
    /// Historical retained main-file identity.
    pub fn database_identity(&self) -> &str {
        &self.database
    }
    /// Original source writer lifecycle.
    pub fn lifecycle_generation(&self) -> u64 {
        self.lifecycle
    }
    /// Original positive physical source lease epoch.
    pub fn lease_epoch(&self) -> u64 {
        self.lease
    }
    /// Original audited semantic source revision.
    pub fn revision(&self) -> u64 {
        self.revision
    }
}
/// Finite original DB/WAL/SHM observations. This is neither a volume claim nor a commit permit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceLayoutAllocationObservation {
    page_size: u64,
    page_count: u64,
    database_bytes: u64,
    maximum_pages: u64,
    wal_bytes: u64,
    shm_bytes: u64,
    prior_bytes: u64,
    prior_digest: Option<[u8; 32]>,
    peak: u64,
}
impl SourceLayoutAllocationObservation {
    /// Measured SQLite page size.
    pub fn page_size(&self) -> u64 {
        self.page_size
    }
    /// Measured logical pages, distinct from retained main-file size.
    pub fn page_count(&self) -> u64 {
        self.page_count
    }
    /// Actual original retained main-file size.
    pub fn resident_database_bytes(&self) -> u64 {
        self.database_bytes
    }
    /// The maximum pages the future entered policy must actually enforce.
    pub fn maximum_pages(&self) -> u64 {
        self.maximum_pages
    }
    /// Original retained WAL size; ordinary hard-watermark rules remain mandatory.
    pub fn resident_wal_bytes(&self) -> u64 {
        self.wal_bytes
    }
    /// Original retained-root WAL-index size.
    pub fn resident_shm_bytes(&self) -> u64 {
        self.shm_bytes
    }
    /// Exact old fixed-key byte length, never semantic generation adoption.
    pub fn prior_generation_bytes(&self) -> u64 {
        self.prior_bytes
    }
    /// Expected exact old byte digest; absence denotes an absent row, not an empty value.
    pub fn prior_generation_digest(&self) -> Option<[u8; 32]> {
        self.prior_digest
    }
    /// Conservative complete source DB/WAL/SHM peak, without journal/profile/native claims.
    pub fn required_metadata_bytes(&self) -> u64 {
        self.peak
    }
    fn validate(&self) -> Result<(), SourceLayoutIntentError> {
        use SourceLayoutIntentError::Allocation as E;
        if !(512..=65536).contains(&self.page_size)
            || !self.page_size.is_power_of_two()
            || self.page_count == 0
            || self.page_count > self.maximum_pages
            || self.shm_bytes < 32768
            || self.shm_bytes % 32768 != 0
            || self.wal_bytes >= crate::WAL_HARD_WATERMARK_BYTES
            || self.prior_bytes > 512 * 1024
            || (self.prior_bytes == 0) != self.prior_digest.is_none()
        {
            return Err(E);
        }
        let database = self.page_size.checked_mul(self.maximum_pages).ok_or(E)?;
        if database > 64 * 1024 * 1024 || self.database_bytes > database {
            return Err(E);
        }
        let wal = self
            .maximum_pages
            .checked_mul(self.page_size.checked_add(24).ok_or(E)?)
            .and_then(|n| n.checked_add(32))
            .ok_or(E)?;
        let shm = self
            .maximum_pages
            .checked_mul(16)
            .and_then(|n| n.checked_add(32768))
            .ok_or(E)?;
        let peak = database
            .checked_add(self.wal_bytes)
            .and_then(|n| n.checked_add(wal))
            .and_then(|n| n.checked_add(self.shm_bytes))
            .and_then(|n| n.checked_add(shm))
            .ok_or(E)?;
        if peak != self.peak {
            return Err(E);
        }
        Ok(())
    }
}
/// Decoded historical data; neither variant carries original source custody or journal authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StoredSourceLayoutIntent {
    /// Complete canonical supported historical input, still requiring actual owner admission.
    Supported(SourceLayoutIntentArgs),
    /// Unknown version retained exactly within the wire byte cap; never interpreted as a grant.
    Unsupported {
        /// Future envelope version.
        version: u64,
        /// Original bounded binary data.
        bytes: Vec<u8>,
    },
}

impl SourceLayoutIntentArgs {
    /// Validate exactly one required source SQL participant and the complete historical fence.
    /// Raw media-root bytes remain in the binary command; the participant binds the retained
    /// domain-separated digest of the raw media-root bytes, so arbitrary opaque roots need
    /// no lossy text conversion. The complete root and database-root binding remain in the command.
    pub fn validate_against_participants(
        &self,
        participants: &super::ParticipantSet,
    ) -> Result<(), SourceLayoutIntentError> {
        self.validate()?;
        participants
            .validate_for_new_intent()
            .map_err(|_| SourceLayoutIntentError::Identity)?;
        if participants.descriptors().len() != 1 {
            return Err(SourceLayoutIntentError::Identity);
        }
        let d = &participants.descriptors()[0];
        let f = d.source_fence().ok_or(SourceLayoutIntentError::Identity)?;
        if d.id() != self.participant
            || d.kind() != super::ParticipantKind::SourceDatabase
            || d.requirement() != super::ParticipantRequirement::Required
            || f.source_id().as_str() != self.scope.source
            || f.root_identity() != self.participant_root_identity()
            || f.database_identity() != self.participant_database_identity()
            || f.lifecycle_generation() != self.scope.lifecycle
            || f.lease_epoch() != self.scope.lease
            || f.source_revision() != self.scope.revision
        {
            return Err(SourceLayoutIntentError::Identity);
        }
        Ok(())
    }
}

// One canonical binary request in a JSON string, never a decimal byte array. Decode only
// historical data; successful serde does not reacquire the producer's writer/capture/budget.
impl serde::Serialize for SourceLayoutIntentArgs {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use base64::Engine;
        let bytes = self.canonical_bytes().map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    }
}
impl<'de> serde::Deserialize<'de> for SourceLayoutIntentArgs {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct HistoricalVisitor;
        impl serde::de::Visitor<'_> for HistoricalVisitor {
            type Value = SourceLayoutIntentArgs;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("bounded canonical base64 source layout input")
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                use base64::Engine;
                if value.len() > 4 * MAX_SOURCE_LAYOUT_INTENT_BYTES.div_ceil(3) {
                    return Err(E::custom("source layout transport exceeds byte bound"));
                }
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(value)
                    .map_err(E::custom)?;
                if base64::engine::general_purpose::STANDARD.encode(&bytes) != value {
                    return Err(E::custom("noncanonical source layout transport"));
                }
                match decode_source_layout_intent(&bytes).map_err(E::custom)? {
                    StoredSourceLayoutIntent::Supported(args) => Ok(args),
                    StoredSourceLayoutIntent::Unsupported { .. } => {
                        Err(E::custom("unsupported source layout input version"))
                    }
                }
            }
        }
        deserializer.deserialize_str(HistoricalVisitor)
    }
}

impl SourceLayoutIntentArgs {
    fn participant_root_identity(&self) -> String {
        let mut h = blake3::Hasher::new();
        h.update(b"yield.source-layout-participant-root.v1\0");
        h.update(&self.scope.root);
        format!("layout-root-v1:{}", h.finalize().to_hex())
    }
    /// Derive the sole historical source participant; this never acquires a live source owner.
    /// The command keeps raw root bytes and the independent database-directory binding exactly.
    pub fn participant_observation(
        &self,
    ) -> Result<super::ParticipantSet, SourceLayoutIntentError> {
        use super::{
            ParticipantCheckpoint, ParticipantCheckpointState, ParticipantDescriptor,
            ParticipantKind, ParticipantRequirement, ParticipantSet, StoredSourceFence,
        };
        self.validate()?;
        let r = &self.scope;
        let fence = StoredSourceFence::new(
            crate::sample_sources::SourceId::from_string(&r.source),
            self.participant_root_identity(),
            self.participant_database_identity(),
            r.lifecycle,
            r.revision,
            r.lease,
        )
        .map_err(|_| SourceLayoutIntentError::Identity)?;
        let descriptor = ParticipantDescriptor::new(
            self.participant,
            ParticipantKind::SourceDatabase,
            ParticipantRequirement::Required,
            Some(fence),
            None,
        )
        .map_err(|_| SourceLayoutIntentError::Identity)?;
        let checkpoint = ParticipantCheckpoint::new(
            self.participant,
            ParticipantCheckpointState::Pending,
            None,
            None,
        )
        .map_err(|_| SourceLayoutIntentError::Identity)?;
        ParticipantSet::new(vec![descriptor], vec![checkpoint])
            .map_err(|_| SourceLayoutIntentError::Identity)
    }
}

impl SourceLayoutIntentArgs {
    fn participant_database_identity(&self) -> String {
        let mut h = blake3::Hasher::new();
        h.update(b"yield.source-layout-participant-database.v1\0");
        for bytes in [
            self.scope.root_binding.as_bytes(),
            self.scope.database.as_bytes(),
        ] {
            h.update(&(bytes.len() as u64).to_le_bytes());
            h.update(bytes);
        }
        format!("layout-db-v1:{}", h.finalize().to_hex())
    }
}
