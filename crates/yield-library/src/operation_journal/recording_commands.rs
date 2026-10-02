//! Finite recording observations and immutable captured bytes, never live admission.
//!
//! This codec does not persist records, authorize dispatch or qualify publication. The journal
//! owner/store bridge must retain these actual bytes before issuing any live chunk grant.
use super::codec::{CappedWriter, parse_unique_value, validate_json_depth};
use super::coordinator::StoredRelativePath;
use super::model::OperationId;
use super::namespace_commands::{CreateFolderIntentArgs, NamespaceTarget};
use super::participants::{ParticipantKind, ParticipantRequirement, ParticipantSet};
use super::work_plan::{WorkUnitKey, WorkUnitRole};
use crate::file_operation_owner::{OutputIdentity, RootRegistrationRef};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Bounded reconstruction and seal observations; no live closure or filesystem authority.
pub mod prefix;

/// Version of this separate bounded observation codec, not an operation wire-schema version.
pub const RECORDING_VALUE_VERSION: u16 = 1;
/// Maximum immutable PCM bytes in one retained chunk.
pub const MAX_RECORDING_CHUNK_BYTES: usize = 16_384;
/// Maximum header extent or patch bytes.
pub const MAX_RECORDING_HEADER_BYTES: usize = 128;
/// Maximum PCM slots per finite capture; owner retention must account for all slots.
pub const MAX_RECORDING_PCM_CHUNKS: u32 = 8192 - MAX_RECORDING_HEADER_CHUNKS;
/// Header replacement slots, including initial header; no unrestricted seek-write graph.
pub const MAX_RECORDING_HEADER_CHUNKS: u32 = 16;
/// Encoded chunk bound includes worst-case JSON byte arrays and the frozen context.
pub const MAX_RECORDING_VALUE_BYTES: usize = 128 * 1024;
/// Hard logical database cap while a recording retention transaction enters SQLite.
pub const MAX_RECORDING_JOURNAL_DATABASE_BYTES: u64 = 512 * 1024 * 1024;
/// Global retained encoded capture budget; exhaustion retains existing prefixes.
pub const MAX_RECORDING_RETAINED_ENCODED_BYTES: u64 = 128 * 1024 * 1024;
/// Conservative complete database plus DELETE rollback-journal peak for bundled SQLite 3.50.2.
/// With cache spill disabled, no savepoints/triggers/extensions and MEMORY temp storage, the
/// old-page journal has at most one record per old page (8 bytes plus page bytes). SQLite's
/// sector-size clamp is 65536; two sectors cover the header/alignment allowance. Existing
/// database allocation is deliberately counted again rather than credited as free capacity.
pub const RECORDING_JOURNAL_PEAK_BYTES: u64 = MAX_RECORDING_JOURNAL_DATABASE_BYTES * 2
    + (MAX_RECORDING_JOURNAL_DATABASE_BYTES / 512) * 8
    + 2 * 65536;

/// Rejected untrusted observations. No positive filesystem outcome is encoded by this API.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("invalid recording observation: {0}")]
pub struct RecordingValueError(String);
fn invalid(detail: impl ToString) -> RecordingValueError {
    RecordingValueError(detail.to_string())
}

/// Historical root registration observation; decoding cannot reconstruct a live registration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecordingRootObservation {
    owner: Uuid,
    registration: Uuid,
    generation: u64,
}
impl From<RootRegistrationRef> for RecordingRootObservation {
    fn from(root: RootRegistrationRef) -> Self {
        Self {
            owner: root.owner_identity().as_uuid(),
            registration: root.registration_id(),
            generation: root.generation(),
        }
    }
}

/// Frozen proposal including participant, namespace and finite allocation observations.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecordingProposal {
    version: u16,
    operation_id: OperationId,
    output_id: Uuid,
    work_key: WorkUnitKey,
    participants: ParticipantSet,
    destination_root: RecordingRootObservation,
    staging_root: RecordingRootObservation,
    destination: NamespaceTarget,
    staging_path: StoredRelativePath,
    format: RecordingFormat,
    collision: RecordingCollision,
}
/// First recording format is interleaved little-endian float32 WAV only.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecordingFormat {
    sample_rate: u32,
    channels: u16,
    max_frames: u64,
    header_bytes: u16,
}
/// Only captured absence/no-replace is representable; no overwrite fallback.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum RecordingCollision {
    RequireAbsentNoReplace,
}
impl RecordingFormat {
    /// Validate before allocating rings, durable storage or a staging object.
    pub fn new(
        sample_rate: u32,
        channels: u16,
        max_frames: u64,
        header_bytes: u16,
    ) -> Result<Self, RecordingValueError> {
        let format = Self {
            sample_rate,
            channels,
            max_frames,
            header_bytes,
        };
        format.validate()?;
        Ok(format)
    }
    /// Checked finite allocation, conservatively including 128 header bytes.
    pub fn max_encoded_bytes(self) -> Result<u64, RecordingValueError> {
        self.max_frames
            .checked_mul(u64::from(self.channels))
            .and_then(|n| n.checked_mul(4))
            .and_then(|n| n.checked_add(MAX_RECORDING_HEADER_BYTES as u64))
            .filter(|n| *n <= u32::MAX as u64)
            .ok_or_else(|| invalid("RIFF allocation overflow"))
    }
    fn frame_bytes(self) -> u64 {
        u64::from(self.channels) * 4
    }
    pub(crate) fn pcm_chunk_bytes(self) -> u64 {
        MAX_RECORDING_CHUNK_BYTES as u64 / self.frame_bytes() * self.frame_bytes()
    }
    /// Number of complete-frame PCM slots in the frozen maximum graph.
    pub fn pcm_slots(self) -> Result<u32, RecordingValueError> {
        if self.channels == 0 || self.channels > 8 {
            return Err(invalid("invalid channels"));
        }
        let bytes = self.max_encoded_bytes()? - MAX_RECORDING_HEADER_BYTES as u64;
        let slots = bytes.div_ceil(self.pcm_chunk_bytes());
        u32::try_from(slots)
            .ok()
            .filter(|n| *n > 0 && *n <= MAX_RECORDING_PCM_CHUNKS)
            .ok_or_else(|| invalid("finite retained chunk graph exceeds budget"))
    }
    /// Validate recovered format values, including the durable chunk graph budget.
    pub fn validate(self) -> Result<(), RecordingValueError> {
        if !(1..=192_000).contains(&self.sample_rate)
            || !(1..=8).contains(&self.channels)
            || self.max_frames == 0
            || !(44..=128).contains(&self.header_bytes)
        {
            return Err(invalid("unsupported finite float32 WAV format"));
        }
        self.max_encoded_bytes()?;
        self.pcm_slots()?;
        Ok(())
    }
}
impl RecordingProposal {
    /// Freeze actual owner observations; none of these values substitutes for a live grant.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        operation_id: OperationId,
        output: OutputIdentity,
        work_key: WorkUnitKey,
        participants: ParticipantSet,
        destination_root: RootRegistrationRef,
        staging_root: RootRegistrationRef,
        destination: NamespaceTarget,
        staging_path: StoredRelativePath,
        format: RecordingFormat,
    ) -> Result<Self, RecordingValueError> {
        let proposal = Self {
            version: RECORDING_VALUE_VERSION,
            operation_id,
            output_id: output.as_uuid(),
            work_key,
            participants,
            destination_root: destination_root.into(),
            staging_root: staging_root.into(),
            destination,
            staging_path,
            format,
            collision: RecordingCollision::RequireAbsentNoReplace,
        };
        proposal.validate()?;
        Ok(proposal)
    }
    /// Validate all frozen observations without opening a root or acquiring authority.
    pub fn validate(&self) -> Result<(), RecordingValueError> {
        if self.version != RECORDING_VALUE_VERSION
            || self.operation_id.as_uuid().is_nil()
            || self.output_id.is_nil()
            || self.work_key
                != WorkUnitKey::new(0, 30, self.work_key.participant_id(), WorkUnitRole::Stage)
        {
            return Err(invalid("version or frozen recording identity"));
        }
        for root in [&self.destination_root, &self.staging_root] {
            if root.owner.is_nil() || root.registration.is_nil() || root.generation == 0 {
                return Err(invalid("root registration observation"));
            }
        }
        if self.destination_root.owner != self.staging_root.owner {
            return Err(invalid("staging and destination have different owners"));
        }
        self.participants
            .validate_for_new_intent()
            .map_err(invalid)?;
        let filesystem = self
            .participants
            .descriptors()
            .iter()
            .find(|p| p.id() == self.work_key.participant_id())
            .ok_or_else(|| invalid("unknown work participant"))?;
        if filesystem.kind() != ParticipantKind::Filesystem
            || filesystem.requirement() != ParticipantRequirement::Required
            || filesystem.source_fence().is_some()
        {
            return Err(invalid(
                "work key is not the required filesystem participant",
            ));
        }
        // Reuse the existing source-parent, leaf, participant and composed-path validators;
        // this does not turn file creation into folder creation or admit either effect.
        CreateFolderIntentArgs::new(
            self.destination.parent().clone(),
            self.destination.leaf().clone(),
        )
        .map_err(invalid)?
        .validate_against_participants(&self.participants)
        .map_err(invalid)?;
        self.staging_path.validate().map_err(invalid)?;
        if self.destination_root == self.staging_root {
            let parent = self
                .destination
                .parent()
                .locator()
                .child_path()
                .map(|p| p.as_str());
            let final_path = parent
                .map(|p| format!("{p}/{}", self.destination.leaf().as_str()))
                .unwrap_or_else(|| self.destination.leaf().as_str().to_owned());
            if final_path == self.staging_path.as_str() {
                return Err(invalid("staging aliases final target"));
            }
        }
        self.format.validate()?;
        encode_value(self).map(|_| ())
    }
    /// Validate this frozen inventory against the enclosing initial-intent participants.
    pub fn validate_against_participants(
        &self,
        participants: &ParticipantSet,
    ) -> Result<(), RecordingValueError> {
        self.validate()?;
        if self.participants() != participants {
            return Err(invalid("recording participant inventory mismatch"));
        }
        Ok(())
    }
    pub(crate) fn participants(&self) -> &ParticipantSet {
        &self.participants
    }
    pub(crate) fn work_key(&self) -> WorkUnitKey {
        self.work_key
    }
    pub(crate) fn destination(&self) -> &NamespaceTarget {
        &self.destination
    }
    pub(crate) fn retained_metadata_bound(&self) -> Result<u64, RecordingValueError> {
        // SQLite page/rollback growth still requires enforced journal metadata admission;
        // this is only the exact finite maximum encoded payload inventory.
        u64::from(self.format.pcm_slots()? + MAX_RECORDING_HEADER_CHUNKS)
            .checked_mul(MAX_RECORDING_VALUE_BYTES as u64)
            .ok_or_else(|| invalid("retained metadata inventory overflow"))
    }
    /// Encode a bounded proposal; caller must persist it through the actual journal owner.
    pub fn encode(&self) -> Result<Vec<u8>, RecordingValueError> {
        self.validate()?;
        encode_value(self)
    }
    /// Strict recovered proposal decoding, never positive admission.
    pub fn decode(bytes: &[u8]) -> Result<Self, RecordingValueError> {
        let value: Self = decode_value(bytes)?;
        value.validate()?;
        Ok(value)
    }
    pub(crate) fn matches_staging_root(&self, root: RootRegistrationRef) -> bool {
        self.staging_root == RecordingRootObservation::from(root)
    }
    /// Frozen stable operation identity.
    pub fn operation_id(&self) -> OperationId {
        self.operation_id
    }
    /// Frozen finite format.
    pub fn format(&self) -> RecordingFormat {
        self.format
    }
}

/// Explicit interrupted-prefix state; no terminal published/qualified state is available here.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum RecordingCaptureDisposition {
    /// Input may still arrive; no publication or completed prefix is established.
    CapturingUnpublished,
    /// Capture stopped; physical validation/publication remains unconfirmed.
    StoppedUnpublished,
    /// Cancelled prefix remains retained until owner audit/retirement permits reclamation.
    CancelledRetained,
    /// Interrupted or lost-result capture needs owner recovery/audit.
    UnconfirmedNeedsAudit,
}
/// Byte role within the frozen graph. Header revisions occupy separately bounded ordered slots.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum RecordingChunkRole {
    /// One complete-frame PCM slot in the maximum frozen graph.
    Pcm {
        /// Zero-based slot index, with deterministic logical offset.
        ordinal: u32,
    },
    /// One bounded header patch, ordered by revision by the later owner bridge.
    Header {
        /// Zero-based immutable header patch slot.
        revision: u32,
    },
}
/// Actual immutable byte fact. A digest alone cannot stand in for unavailable microphone input.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecordingCapturedChunk {
    version: u16,
    proposal_hash: [u8; 32],
    operation_id: OperationId,
    output_id: Uuid,
    work_key: WorkUnitKey,
    chunk_id: Uuid,
    role: RecordingChunkRole,
    offset: u64,
    bytes: Vec<u8>,
}
impl RecordingCapturedChunk {
    /// Capture immutable encoded bytes with exact proposal binding; retention is a later step.
    pub fn new(
        proposal: &RecordingProposal,
        chunk_id: Uuid,
        role: RecordingChunkRole,
        offset: u64,
        bytes: Vec<u8>,
    ) -> Result<Self, RecordingValueError> {
        let chunk = Self {
            version: RECORDING_VALUE_VERSION,
            proposal_hash: *blake3::hash(&proposal.encode()?).as_bytes(),
            operation_id: proposal.operation_id,
            output_id: proposal.output_id,
            work_key: proposal.work_key,
            chunk_id,
            role,
            offset,
            bytes,
        };
        chunk.validate_against(proposal)?;
        Ok(chunk)
    }
    /// Check exact frozen identities, complete frames and bounded slot/offset graph.
    pub fn validate_against(
        &self,
        proposal: &RecordingProposal,
    ) -> Result<(), RecordingValueError> {
        let hash = *blake3::hash(&proposal.encode()?).as_bytes();
        if self.version != RECORDING_VALUE_VERSION
            || self.chunk_id.is_nil()
            || self.proposal_hash != hash
            || self.operation_id != proposal.operation_id
            || self.output_id != proposal.output_id
            || self.work_key != proposal.work_key
            || self.bytes.is_empty()
        {
            return Err(invalid("captured chunk binding/version"));
        }
        let format = proposal.format;
        let end = self
            .offset
            .checked_add(self.bytes.len() as u64)
            .ok_or_else(|| invalid("chunk offset overflow"))?;
        match self.role {
            RecordingChunkRole::Header { revision } => {
                if (revision == 0
                    && (self.offset != 0 || self.bytes.len() != usize::from(format.header_bytes)))
                    || revision >= MAX_RECORDING_HEADER_CHUNKS
                    || self.bytes.len() > MAX_RECORDING_HEADER_BYTES
                    || end > u64::from(format.header_bytes)
                {
                    return Err(invalid("header patch graph bound"));
                }
            }
            RecordingChunkRole::Pcm { ordinal } => {
                let chunk_bytes = format.pcm_chunk_bytes();
                let expected = u64::from(format.header_bytes) + u64::from(ordinal) * chunk_bytes;
                let pcm_end = u64::from(format.header_bytes) + format.max_encoded_bytes()?
                    - MAX_RECORDING_HEADER_BYTES as u64;
                if ordinal >= format.pcm_slots()?
                    || self.offset != expected
                    || self.bytes.len() as u64 > chunk_bytes
                    || self.bytes.len() as u64 % format.frame_bytes() != 0
                    || end > pcm_end
                {
                    return Err(invalid("PCM frame/slot/allocation bound"));
                }
            }
        }
        Ok(())
    }
    /// Encode only after validating against the exact frozen proposal.
    pub fn encode(&self, proposal: &RecordingProposal) -> Result<Vec<u8>, RecordingValueError> {
        self.validate_against(proposal)?;
        encode_value(self)
    }
    /// Decode immutable bytes; unknown versions/roles remain errors for owner retention/audit.
    pub fn decode(bytes: &[u8], proposal: &RecordingProposal) -> Result<Self, RecordingValueError> {
        let value: Self = decode_value(bytes)?;
        value.validate_against(proposal)?;
        Ok(value)
    }
    /// Accept only exact immutable replay; changed bytes or frozen identities are a conflict.
    /// The journal store must enforce this before any dispatch or durable replacement.
    pub fn validate_replay(
        &self,
        incoming: &Self,
        proposal: &RecordingProposal,
    ) -> Result<(), RecordingValueError> {
        self.validate_against(proposal)?;
        incoming.validate_against(proposal)?;
        if self == incoming {
            Ok(())
        } else {
            Err(invalid("conflicting immutable captured chunk replay"))
        }
    }
    pub(crate) fn output_id(&self) -> Uuid {
        self.output_id
    }
    pub(crate) fn work_key(&self) -> WorkUnitKey {
        self.work_key
    }
    pub(crate) fn offset(&self) -> u64 {
        self.offset
    }
    pub(crate) fn chunk_id(&self) -> Uuid {
        self.chunk_id
    }
    pub(crate) fn operation_id(&self) -> OperationId {
        self.operation_id
    }
    pub(crate) fn role(&self) -> RecordingChunkRole {
        self.role
    }
    /// Exact recorded bytes for a later admitted adapter; borrowing confers no dispatch grant.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}
fn encode_value(value: &impl Serialize) -> Result<Vec<u8>, RecordingValueError> {
    let mut writer = CappedWriter::new(MAX_RECORDING_VALUE_BYTES);
    serde_json::to_writer(&mut writer, value).map_err(invalid)?;
    Ok(writer.finish())
}
fn decode_value<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, RecordingValueError> {
    if bytes.len() > MAX_RECORDING_VALUE_BYTES {
        return Err(invalid("encoded value exceeds bound"));
    }
    let value = parse_unique_value(bytes).map_err(invalid)?;
    validate_json_depth(&value, 24).map_err(invalid)?;
    serde_json::from_value(value).map_err(invalid)
}
#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) fn recording_proposal_for_test(channels: u16, frames: u64) -> RecordingProposal {
    tests::proposal(channels, frames)
}

#[cfg(test)]
pub(super) fn recording_proposal_with_root_for_test() -> (RecordingProposal, RootRegistrationRef) {
    tests::proposal_with_root(2, 100)
}
