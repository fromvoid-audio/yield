//! Actual WAV-byte reconstruction, not admission, durable closure or physical completion.
//!
//! OPT-2145 must bind these observations to its immutable SQLite inventory and live terminal
//! capability. Caller-provided chunks/accounting and decoded seals cannot issue a grant.
use super::*;

/// Supported hound float32 extensible header extent.
pub const RECORDING_WAV_HEADER_BYTES: usize = 68;
/// Preserve four terminal-control slots within the existing recording row ceiling.
pub const MAX_SEALED_RECORDING_PCM_CHUNKS: u32 = 8172;
/// Maximum input chunks in one verifier turn.
pub const MAX_PREFIX_PAGE_CHUNKS: usize = 8;
const MAX_CONTROL_BYTES: usize = 4096;
const PREFIX_VERSION: u16 = 1;

/// Closed producer observations. Live snapshots and these fields confer no authority.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecordingSampleAccounting {
    /// Samples accepted into the writer ring.
    pub accepted: u64,
    /// Samples in complete successful encoder batches.
    pub encoded: u64,
    /// Accepted samples abandoned without an encoding attempt.
    pub abandoned: u64,
    /// Samples in a failed batch whose exact encoding is unknown.
    pub unconfirmed: u64,
    /// Rejected input samples, separately preserved from accepted samples.
    pub rejected: u64,
    /// Input writer overrun observations, excluding monitor-only loss.
    pub overruns: u64,
    /// The actual callback/worker sample partition has settled.
    pub complete: bool,
    /// Any relevant counter saturated or overflowed.
    pub overflowed: bool,
    /// The real writer join and finalization returned success.
    pub joined_successfully: bool,
    /// The writer reported an error, even if its accounting partition settled.
    pub writer_failed: bool,
}

/// Requested terminal observation, never a publication disposition.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum RecordingTerminalKind {
    /// Successful Stop requires every accepted sample to be encoded.
    Stop,
    /// Cancel may abandon accepted samples but remains unpublished.
    Cancel,
}

/// Immutable frozen-inventory observation; cannot reconstruct a live terminal capability.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecordingPrefixInventory {
    version: u16,
    proposal_hash: [u8; 32],
    operation_id: OperationId,
    output_id: Uuid,
    work_key: WorkUnitKey,
    terminal_id: Uuid,
    freeze_sequence: u64,
    generation: u64,
    last_header_revision: u32,
    terminal: RecordingTerminalKind,
    accounting: RecordingSampleAccounting,
}

impl RecordingPrefixInventory {
    /// Capture bounded frozen metadata for the later same-owner retention bridge.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        proposal: &RecordingProposal,
        terminal_id: Uuid,
        freeze_sequence: u64,
        generation: u64,
        last_header_revision: u32,
        terminal: RecordingTerminalKind,
        accounting: RecordingSampleAccounting,
    ) -> Result<Self, RecordingValueError> {
        let value = Self {
            version: PREFIX_VERSION,
            proposal_hash: *blake3::hash(&proposal.encode()?).as_bytes(),
            operation_id: proposal.operation_id,
            output_id: proposal.output_id,
            work_key: proposal.work_key,
            terminal_id,
            freeze_sequence,
            generation,
            last_header_revision,
            terminal,
            accounting,
        };
        value.validate(proposal)?;
        Ok(value)
    }

    fn validate(&self, proposal: &RecordingProposal) -> Result<(), RecordingValueError> {
        proposal.validate()?;
        if self.version != PREFIX_VERSION
            || self.proposal_hash != *blake3::hash(&proposal.encode()?).as_bytes()
            || self.operation_id != proposal.operation_id
            || self.output_id != proposal.output_id
            || self.work_key != proposal.work_key
            || self.terminal_id.is_nil()
            || self.freeze_sequence == 0
            || self.freeze_sequence > i64::MAX as u64
            || self.generation == 0
            || !(1..MAX_RECORDING_HEADER_CHUNKS).contains(&self.last_header_revision)
            || usize::from(proposal.format.header_bytes) != RECORDING_WAV_HEADER_BYTES
            || proposal.format.pcm_slots()? > MAX_SEALED_RECORDING_PCM_CHUNKS
        {
            return Err(invalid(
                "unsupported or conflicting frozen prefix inventory",
            ));
        }
        let a = self.accounting;
        let accounted = a
            .encoded
            .checked_add(a.abandoned)
            .and_then(|n| n.checked_add(a.unconfirmed));
        if !a.complete
            || a.overflowed
            || !a.joined_successfully
            || a.writer_failed
            || a.unconfirmed != 0
            || accounted != Some(a.accepted)
            || [a.accepted, a.encoded, a.abandoned, a.unconfirmed]
                .into_iter()
                .any(|n| n % u64::from(proposal.format.channels) != 0)
            || (self.terminal == RecordingTerminalKind::Stop && a.abandoned != 0)
            || self.frames(proposal) > proposal.format.max_frames
        {
            return Err(invalid(
                "incomplete, failed or conflicting terminal accounting",
            ));
        }
        self.pcm_bytes()?;
        Ok(())
    }

    fn frames(&self, proposal: &RecordingProposal) -> u64 {
        self.accounting.encoded / u64::from(proposal.format.channels)
    }

    fn pcm_bytes(&self) -> Result<u64, RecordingValueError> {
        self.accounting
            .encoded
            .checked_mul(4)
            .ok_or_else(|| invalid("encoded PCM byte overflow"))
    }

    fn pcm_chunks(&self, proposal: &RecordingProposal) -> Result<u32, RecordingValueError> {
        u32::try_from(
            self.pcm_bytes()?
                .div_ceil(proposal.format.pcm_chunk_bytes()),
        )
        .map_err(invalid)
    }

    /// Strict bounded observation encoding, never a live token.
    pub fn encode(&self, proposal: &RecordingProposal) -> Result<Vec<u8>, RecordingValueError> {
        self.validate(proposal)?;
        control_encode(self)
    }

    /// Decode against the exact immutable proposal; unknown variants fail retained.
    pub fn decode(bytes: &[u8], proposal: &RecordingProposal) -> Result<Self, RecordingValueError> {
        let value: Self = control_decode(bytes)?;
        value.validate(proposal)?;
        Ok(value)
    }

    /// Frozen operation identity; observing it does not establish an acquisition.
    pub fn operation_id(&self) -> OperationId {
        self.operation_id
    }
    /// Exact frozen output identity.
    pub fn output_id(&self) -> Uuid {
        self.output_id
    }
    /// Exact frozen filesystem work key.
    pub fn work_key(&self) -> WorkUnitKey {
        self.work_key
    }
    /// Frozen terminal request identity; not a live token.
    pub fn terminal_id(&self) -> Uuid {
        self.terminal_id
    }
    /// Positive journal sequence of the immutable inventory.
    pub fn freeze_sequence(&self) -> u64 {
        self.freeze_sequence
    }
    /// Exact frozen inventory generation, not a source lease.
    pub fn generation(&self) -> u64 {
        self.generation
    }
    /// Last immutable final-header revision required by this inventory.
    pub fn last_header_revision(&self) -> u32 {
        self.last_header_revision
    }
    /// Stop or Cancel observation; Cancel cannot authorize publication.
    pub fn terminal(&self) -> RecordingTerminalKind {
        self.terminal
    }
    /// Complete frozen sample observations, distinct from actual durable byte coverage.
    pub fn accounting(&self) -> RecordingSampleAccounting {
        self.accounting
    }
}

/// Fold the real encoder's initial writes and length rewrites into one complete header.
pub struct InitialRecordingHeader {
    proposal: RecordingProposal,
    bytes: [u8; RECORDING_WAV_HEADER_BYTES],
    coverage: u128,
}

impl InitialRecordingHeader {
    /// Start bounded initialization without opening or dispatching an output.
    pub fn new(proposal: &RecordingProposal) -> Result<Self, RecordingValueError> {
        proposal.validate()?;
        if usize::from(proposal.format.header_bytes) != RECORDING_WAV_HEADER_BYTES {
            return Err(invalid("unsupported initial WAV header extent"));
        }
        Ok(Self {
            proposal: proposal.clone(),
            bytes: [0; RECORDING_WAV_HEADER_BYTES],
            coverage: 0,
        })
    }

    /// Apply one actual initialization write, including hound's first flush rewrites.
    pub fn apply_write(&mut self, offset: u64, bytes: &[u8]) -> Result<(), RecordingValueError> {
        let start = usize::try_from(offset).map_err(invalid)?;
        let end = start
            .checked_add(bytes.len())
            .filter(|n| *n <= RECORDING_WAV_HEADER_BYTES)
            .ok_or_else(|| invalid("initial header write outside extent"))?;
        if bytes.is_empty() {
            return Err(invalid("empty initial header write"));
        }
        self.bytes[start..end].copy_from_slice(bytes);
        for index in start..end {
            self.coverage |= 1u128 << index;
        }
        Ok(())
    }

    /// Return one revision-zero observation after all actual zero-frame header bytes validate.
    pub fn finish(self, chunk_id: Uuid) -> Result<RecordingCapturedChunk, RecordingValueError> {
        if self.coverage != (1u128 << RECORDING_WAV_HEADER_BYTES) - 1 {
            return Err(invalid("initial header coverage gap"));
        }
        validate_header(&self.bytes, self.proposal.format, 0)?;
        RecordingCapturedChunk::new(
            &self.proposal,
            chunk_id,
            RecordingChunkRole::Header { revision: 0 },
            0,
            self.bytes.to_vec(),
        )
    }
}

/// Exact process-local page position, bound to the entire frozen inventory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordingPrefixCursor {
    inventory_hash: [u8; 32],
    next_header: u32,
    next_pcm: u32,
}

/// Incremental real-byte verifier. It owns a bounded header/hash, not the full recording.
#[derive(Clone)]
pub struct RecordingPrefixVerifier {
    proposal: RecordingProposal,
    inventory: RecordingPrefixInventory,
    cursor: RecordingPrefixCursor,
    header: [u8; RECORDING_WAV_HEADER_BYTES],
    hash: blake3::Hasher,
    header_verified: bool,
    pcm_bytes: u64,
}

impl RecordingPrefixVerifier {
    /// Start an observation verifier; the durable owner must separately prove Frozen inventory.
    pub fn new(
        proposal: &RecordingProposal,
        inventory: RecordingPrefixInventory,
    ) -> Result<Self, RecordingValueError> {
        let inventory_hash = *blake3::hash(&inventory.encode(proposal)?).as_bytes();
        Ok(Self {
            proposal: proposal.clone(),
            inventory,
            cursor: RecordingPrefixCursor {
                inventory_hash,
                next_header: 0,
                next_pcm: 0,
            },
            header: [0; RECORDING_WAV_HEADER_BYTES],
            hash: blake3::Hasher::new(),
            header_verified: false,
            pcm_bytes: 0,
        })
    }

    /// Current exact page position; stale or other-inventory cursors fail without progress.
    pub fn cursor(&self) -> RecordingPrefixCursor {
        self.cursor
    }

    fn page_check(
        &self,
        cursor: RecordingPrefixCursor,
        count: usize,
    ) -> Result<(), RecordingValueError> {
        if cursor != self.cursor || !(1..=MAX_PREFIX_PAGE_CHUNKS).contains(&count) {
            return Err(invalid(
                "stale/substituted cursor or oversized/empty prefix page",
            ));
        }
        Ok(())
    }

    /// Reconstruct header revisions first. A failed page leaves all verifier state unchanged.
    pub fn header_page(
        &mut self,
        cursor: RecordingPrefixCursor,
        chunks: &[RecordingCapturedChunk],
    ) -> Result<(), RecordingValueError> {
        self.page_check(cursor, chunks.len())?;
        let mut next = self.clone();
        for chunk in chunks {
            chunk.validate_against(&next.proposal)?;
            if next.header_verified
                || chunk.role
                    != (RecordingChunkRole::Header {
                        revision: next.cursor.next_header,
                    })
                || next.cursor.next_header > next.inventory.last_header_revision
            {
                return Err(invalid("header inventory revision gap/conflict"));
            }
            let start = usize::try_from(chunk.offset).map_err(invalid)?;
            let end = start
                .checked_add(chunk.bytes.len())
                .filter(|n| *n <= RECORDING_WAV_HEADER_BYTES)
                .ok_or_else(|| invalid("patch exceeds supported header"))?;
            next.header[start..end].copy_from_slice(&chunk.bytes);
            if next.cursor.next_header == 0 {
                validate_header(&next.header, next.proposal.format, 0)?;
            }
            next.cursor.next_header += 1;
            if next.cursor.next_header == next.inventory.last_header_revision + 1 {
                validate_header(
                    &next.header,
                    next.proposal.format,
                    next.inventory.pcm_bytes()?,
                )?;
                next.hash.update(&next.header);
                next.header_verified = true;
            }
        }
        *self = next;
        Ok(())
    }

    /// Hash exact contiguous PCM bytes after the complete final header, at most eight slots.
    pub fn pcm_page(
        &mut self,
        cursor: RecordingPrefixCursor,
        chunks: &[RecordingCapturedChunk],
    ) -> Result<(), RecordingValueError> {
        self.page_check(cursor, chunks.len())?;
        let mut next = self.clone();
        let count = next.inventory.pcm_chunks(&next.proposal)?;
        let total = next.inventory.pcm_bytes()?;
        for chunk in chunks {
            chunk.validate_against(&next.proposal)?;
            let ordinal = next.cursor.next_pcm;
            let expected_length =
                (total - next.pcm_bytes).min(next.proposal.format.pcm_chunk_bytes());
            if !next.header_verified
                || ordinal >= count
                || chunk.role != (RecordingChunkRole::Pcm { ordinal })
                || chunk.bytes.len() as u64 != expected_length
            {
                return Err(invalid(
                    "PCM coverage/length/order differs from frozen inventory",
                ));
            }
            next.hash.update(&chunk.bytes);
            next.pcm_bytes = next
                .pcm_bytes
                .checked_add(chunk.bytes.len() as u64)
                .ok_or_else(|| invalid("PCM coverage overflow"))?;
            next.cursor.next_pcm += 1;
        }
        *self = next;
        Ok(())
    }

    /// Produce immutable byte observations only after every declared header/PCM slot validates.
    pub fn finish(self) -> Result<RecordingPrefixSeal, RecordingValueError> {
        if !self.header_verified
            || self.cursor.next_pcm != self.inventory.pcm_chunks(&self.proposal)?
            || self.pcm_bytes != self.inventory.pcm_bytes()?
        {
            return Err(invalid("incomplete retained recording prefix"));
        }
        let seal = RecordingPrefixSeal {
            version: PREFIX_VERSION,
            frames: self.inventory.frames(&self.proposal),
            bytes: self.pcm_bytes + RECORDING_WAV_HEADER_BYTES as u64,
            header_hash: *blake3::hash(&self.header).as_bytes(),
            content_hash: *self.hash.finalize().as_bytes(),
            inventory: self.inventory,
        };
        seal.validate(&self.proposal)?;
        Ok(seal)
    }
}

/// Retained-byte seal observation. Decoding cannot mint Frozen state or a physical receipt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecordingPrefixSeal {
    version: u16,
    inventory: RecordingPrefixInventory,
    frames: u64,
    bytes: u64,
    header_hash: [u8; 32],
    content_hash: [u8; 32],
}

impl RecordingPrefixSeal {
    fn validate(&self, proposal: &RecordingProposal) -> Result<(), RecordingValueError> {
        self.inventory.validate(proposal)?;
        if self.version != PREFIX_VERSION
            || self.frames != self.inventory.frames(proposal)
            || self.bytes
                != self
                    .inventory
                    .pcm_bytes()?
                    .checked_add(RECORDING_WAV_HEADER_BYTES as u64)
                    .ok_or_else(|| invalid("sealed length overflow"))?
            || self.header_hash == [0; 32]
            || self.content_hash == [0; 32]
        {
            return Err(invalid("conflicting or unknown sealed byte observations"));
        }
        Ok(())
    }
    /// Observed complete interleaved frame count.
    pub fn frames(&self) -> u64 {
        self.frames
    }
    /// Exact logical WAV length, not an allocation or durability observation.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
    /// BLAKE3 of final reconstructed header followed by actual retained PCM bytes.
    pub fn content_hash(&self) -> [u8; 32] {
        self.content_hash
    }
    /// The complete immutable terminal inventory bound into this observation.
    pub fn inventory(&self) -> &RecordingPrefixInventory {
        &self.inventory
    }
    /// Strict bounded seal observation encoding.
    pub fn encode(&self, proposal: &RecordingProposal) -> Result<Vec<u8>, RecordingValueError> {
        self.validate(proposal)?;
        control_encode(self)
    }
    /// Decode an observation only; production recovery must verify its actual retained bytes.
    pub fn decode(bytes: &[u8], proposal: &RecordingProposal) -> Result<Self, RecordingValueError> {
        let value: Self = control_decode(bytes)?;
        value.validate(proposal)?;
        Ok(value)
    }
    /// Exact immutable replay; changed hash, accounting, frozen context or byte counts conflicts.
    pub fn validate_replay(
        &self,
        incoming: &Self,
        proposal: &RecordingProposal,
    ) -> Result<(), RecordingValueError> {
        self.validate(proposal)?;
        incoming.validate(proposal)?;
        if self == incoming {
            Ok(())
        } else {
            Err(invalid("conflicting immutable prefix seal replay"))
        }
    }
}

fn control_encode(value: &impl Serialize) -> Result<Vec<u8>, RecordingValueError> {
    let bytes = encode_value(value)?;
    if bytes.len() > MAX_CONTROL_BYTES {
        return Err(invalid("prefix control exceeds bound"));
    }
    Ok(bytes)
}
fn control_decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, RecordingValueError> {
    if bytes.len() > MAX_CONTROL_BYTES {
        return Err(invalid("prefix control exceeds bound"));
    }
    decode_value(bytes)
}
fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}
fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("fixed header extent"),
    )
}
fn validate_header(
    header: &[u8; RECORDING_WAV_HEADER_BYTES],
    format: RecordingFormat,
    pcm: u64,
) -> Result<(), RecordingValueError> {
    let pcm = u32::try_from(pcm).map_err(invalid)?;
    let riff = pcm
        .checked_add(60)
        .ok_or_else(|| invalid("RIFF length overflow"))?;
    const FLOAT_GUID: [u8; 16] = [3, 0, 0, 0, 0, 0, 16, 0, 128, 0, 0, 170, 0, 56, 155, 113];
    if &header[..4] != b"RIFF"
        || u32_at(header, 4) != riff
        || &header[8..12] != b"WAVE"
        || &header[12..16] != b"fmt "
        || u32_at(header, 16) != 40
        || u16_at(header, 20) != 0xfffe
        || u16_at(header, 22) != format.channels
        || u32_at(header, 24) != format.sample_rate
        || u32_at(header, 28) != format.sample_rate * u32::from(format.channels) * 4
        || u16_at(header, 32) != format.channels * 4
        || u16_at(header, 34) != 32
        || u16_at(header, 36) != 22
        || u16_at(header, 38) != 32
        || u32_at(header, 40) != (1u32 << format.channels) - 1
        || header[44..60] != FLOAT_GUID
        || &header[60..64] != b"data"
        || u32_at(header, 64) != pcm
    {
        return Err(invalid(
            "actual WAV header conflicts with frozen float32 format/coverage",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
