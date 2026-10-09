//! Bounded data normalization. These values provide no Source, write or close permission.
use super::*;
use std::sync::Arc;

struct Instance;

/// Original contiguous PCM byte cursor, independent of arbitrary buffered write boundaries.
/// The owner must fund this fixed buffer before construction and keep physical admission separate.
pub struct RecordingPcmNormalizer {
    instance: Arc<Instance>,
    proposal_hash: [u8; 32],
    generation: u64,
    next_ordinal: u32,
    raw_offset: u64,
    carry: [u8; MAX_RECORDING_CHUNK_BYTES],
    carry_len: usize,
    closed: bool,
}

/// At most two normalized PCM chunks, two terminal patches and one bounded carry. Applying data is not a write receipt.
pub struct RecordingPcmBatch {
    instance: Arc<Instance>,
    generation: u64,
    raw_offset: u64,
    next_ordinal: u32,
    chunks: Vec<RecordingCapturedChunk>,
    carry: Vec<u8>,
    terminal: bool,
}
impl RecordingPcmBatch {
    /// Actual normalized bytes for a separately admitted retention/physical consumer.
    pub fn chunks(&self) -> &[RecordingCapturedChunk] {
        &self.chunks
    }
}
impl RecordingPcmNormalizer {
    /// Initialize only the finite data grammar; this does not activate a producer.
    pub fn new(proposal: &RecordingProposal) -> Result<Self, RecordingValueError> {
        controls::reservation_metadata_bound(proposal)?;
        Ok(Self::from_proposal_hash(
            *blake3::hash(&proposal.encode()?).as_bytes(),
        ))
    }
    /// Original worker data initialization after its existing proposal validation/encoding.
    /// The hash initializes only a data cursor; it supplies no Source or startup permission.
    pub(crate) fn from_proposal_hash(proposal_hash: [u8; 32]) -> Self {
        Self {
            instance: Arc::new(Instance),
            proposal_hash,
            generation: 1,
            next_ordinal: 0,
            raw_offset: prefix::RECORDING_WAV_HEADER_BYTES as u64,
            carry: [0; MAX_RECORDING_CHUNK_BYTES],
            carry_len: 0,
            closed: false,
        }
    }
    /// Prepare one original <=16-KiB contiguous write without changing the retained cursor.
    /// IDs must be supplied for exactly the complete chunks. The owner retains this batch until
    /// its separate journal/write acknowledgements; refusal can retry the unchanged cursor.
    pub fn prepare(
        &self,
        proposal: &RecordingProposal,
        offset: u64,
        bytes: &[u8],
        ids: &[Uuid],
    ) -> Result<RecordingPcmBatch, RecordingValueError> {
        if self.closed
            || *blake3::hash(&proposal.encode()?).as_bytes() != self.proposal_hash
            || offset != self.raw_offset
            || bytes.is_empty()
            || bytes.len() > MAX_RECORDING_CHUNK_BYTES
            || self.generation == u64::MAX
        {
            return Err(invalid(
                "PCM cursor, proposal, generation or write bound changed",
            ));
        }
        let chunk_bytes = usize::try_from(proposal.format().pcm_chunk_bytes()).map_err(invalid)?;
        let combined = self
            .carry_len
            .checked_add(bytes.len())
            .ok_or_else(|| invalid("PCM carry overflow"))?;
        let complete = combined / chunk_bytes;
        if complete > 2
            || ids.len() != complete
            || ids.iter().any(Uuid::is_nil)
            || (ids.len() == 2 && ids[0] == ids[1])
        {
            return Err(invalid("PCM complete chunk identity inventory changed"));
        }
        let next_ordinal = self
            .next_ordinal
            .checked_add(complete as u32)
            .ok_or_else(|| invalid("PCM ordinal overflow"))?;
        let raw_offset = offset
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| invalid("PCM offset overflow"))?;
        let maximum = prefix::RECORDING_WAV_HEADER_BYTES as u64
            + proposal.format().max_frames() * u64::from(proposal.format().channels()) * 4;
        if raw_offset > maximum || next_ordinal > proposal.format().pcm_slots()? {
            return Err(invalid("PCM data exceeds original finite proposal"));
        }
        let mut chunks = Vec::with_capacity(complete);
        for (index, id) in ids.iter().enumerate() {
            let start = index * chunk_bytes;
            let data = self.combined_range(bytes, start, chunk_bytes);
            let ordinal = self.next_ordinal + index as u32;
            chunks.push(RecordingCapturedChunk::new(
                proposal,
                *id,
                RecordingChunkRole::Pcm { ordinal },
                prefix::RECORDING_WAV_HEADER_BYTES as u64 + u64::from(ordinal) * chunk_bytes as u64,
                data,
            )?);
        }
        let start = complete * chunk_bytes;
        Ok(RecordingPcmBatch {
            instance: Arc::clone(&self.instance),
            generation: self.generation,
            raw_offset,
            next_ordinal,
            chunks,
            carry: self.combined_range(bytes, start, combined - start),
            terminal: false,
        })
    }
    fn combined_range(&self, bytes: &[u8], start: usize, length: usize) -> Vec<u8> {
        let mut output = Vec::with_capacity(length);
        let end = start + length;
        if start < self.carry_len {
            output.extend_from_slice(&self.carry[start..end.min(self.carry_len)]);
        }
        if end > self.carry_len {
            output.extend_from_slice(
                &bytes[start.saturating_sub(self.carry_len)..end - self.carry_len],
            );
        }
        output
    }
    /// Advance this exact data cursor once. This is not physical acknowledgement or admission.
    pub fn accept(&mut self, batch: RecordingPcmBatch) -> Result<(), RecordingValueError> {
        if self.closed
            || !Arc::ptr_eq(&self.instance, &batch.instance)
            || self.generation != batch.generation
        {
            return Err(invalid("foreign or stale original PCM batch"));
        }
        self.carry[..batch.carry.len()].copy_from_slice(&batch.carry);
        self.carry_len = batch.carry.len();
        self.raw_offset = batch.raw_offset;
        self.next_ordinal = batch.next_ordinal;
        self.closed = batch.terminal;
        self.generation += 1;
        Ok(())
    }
    /// Bind the normalized tail and the two header patches to an original producer barrier.
    /// This authenticates byte provenance only; the caller must separately fund, retain and
    /// admit these chunks before acknowledging the producer. No physical authority is returned.
    pub fn prepare_original_terminal(
        &self,
        proposal: &RecordingProposal,
        terminal: Uuid,
        settlements: &reson::recording::RecordingSettlements,
        barrier: &reson::recording::RecordingTerminalBarrier,
    ) -> Result<RecordingPcmBatch, RecordingValueError> {
        let limits = barrier.limits();
        if !settlements.owns_barrier(barrier)
            || limits.sample_rate() != proposal.format().sample_rate()
            || limits.channels() != proposal.format().channels()
            || limits.max_frames() != proposal.format().max_frames()
        {
            return Err(invalid("foreign original terminal producer or format"));
        }
        let inventory = barrier.write_inventory();
        let mut batch = self.prepare_terminal(
            proposal,
            terminal,
            inventory.pending_offset(),
            inventory.pending_bytes(),
            inventory.object_bytes(),
        )?;
        for (revision, offset, patch) in [
            (1, 4, inventory.riff_length_patch()),
            (2, 64, inventory.data_length_patch()),
        ] {
            let id = terminal_chunk_identity(
                &self.proposal_hash,
                terminal,
                RecordingChunkRole::Header { revision },
            );
            batch.chunks.push(RecordingCapturedChunk::new(
                proposal,
                id,
                RecordingChunkRole::Header { revision },
                offset,
                patch.to_vec(),
            )?);
        }
        Ok(batch)
    }

    /// Derive the exact <=2 normalized terminal chunks from original carry and pending bytes.
    /// Caller must authenticate the original barrier; this data plan cannot close a producer.
    pub fn prepare_terminal(
        &self,
        proposal: &RecordingProposal,
        terminal: Uuid,
        pending_offset: u64,
        pending: &[u8],
        object_bytes: u64,
    ) -> Result<RecordingPcmBatch, RecordingValueError> {
        if self.closed
            || self.generation == u64::MAX
            || terminal.is_nil()
            || pending_offset != self.raw_offset
            || pending.len() > MAX_RECORDING_CHUNK_BYTES
            || pending_offset.checked_add(pending.len() as u64) != Some(object_bytes)
            || *blake3::hash(&proposal.encode()?).as_bytes() != self.proposal_hash
        {
            return Err(invalid("terminal PCM differs from original data custody"));
        }
        let combined = self.carry_len + pending.len();
        let chunk_bytes = proposal.format().pcm_chunk_bytes() as usize;
        if combined % (usize::from(proposal.format().channels()) * 4) != 0
            || combined.div_ceil(chunk_bytes) > 2
        {
            return Err(invalid(
                "terminal PCM has incomplete frames or exceeds bound",
            ));
        }
        let mut chunks = Vec::with_capacity(combined.div_ceil(chunk_bytes));
        for start in (0..combined).step_by(chunk_bytes) {
            let ordinal = self.next_ordinal + (start / chunk_bytes) as u32;
            let id = terminal_chunk_identity(
                &self.proposal_hash,
                terminal,
                RecordingChunkRole::Pcm { ordinal },
            );
            chunks.push(RecordingCapturedChunk::new(
                proposal,
                id,
                RecordingChunkRole::Pcm { ordinal },
                prefix::RECORDING_WAV_HEADER_BYTES as u64 + u64::from(ordinal) * chunk_bytes as u64,
                self.combined_range(pending, start, (combined - start).min(chunk_bytes)),
            )?);
        }
        let next_ordinal = self
            .next_ordinal
            .checked_add(chunks.len() as u32)
            .ok_or_else(|| invalid("terminal PCM ordinal overflow"))?;
        Ok(RecordingPcmBatch {
            instance: Arc::clone(&self.instance),
            generation: self.generation,
            raw_offset: object_bytes,
            next_ordinal,
            chunks,
            carry: Vec::new(),
            terminal: true,
        })
    }
}

#[cfg(test)]
mod tests;

/// Stable data identity; it cannot issue a chunk grant.
pub(super) fn terminal_chunk_identity(
    proposal_hash: &[u8; 32],
    terminal: Uuid,
    role: RecordingChunkRole,
) -> Uuid {
    let mut hash = blake3::Hasher::new();
    let ordinal = match role {
        RecordingChunkRole::Pcm { ordinal } => {
            hash.update(b"yield.recording.terminal.pcm.v1\0");
            ordinal
        }
        RecordingChunkRole::Header { revision } => {
            hash.update(b"yield.recording.terminal.header.v1\0");
            revision
        }
    };
    hash.update(proposal_hash);
    hash.update(terminal.as_bytes());
    hash.update(&ordinal.to_le_bytes());
    let mut id = [0; 16];
    id.copy_from_slice(&hash.finalize().as_bytes()[..16]);
    Uuid::from_bytes(id)
}
