//! Closed normalized terminal declarations, never retained-write or closure authority.
use super::terminal::RecordingFinalizingInventory;
use super::*;
use reson::recording::{RecordingSettlements, RecordingTerminalBarrier};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct TerminalChunkDeclaration {
    id: Uuid,
    role: RecordingChunkRole,
    offset: u64,
    length: u32,
    hash: [u8; 32],
}
impl TerminalChunkDeclaration {
    fn from_chunk(chunk: &RecordingCapturedChunk) -> Result<Self, RecordingValueError> {
        Ok(Self {
            id: chunk.chunk_id(),
            role: chunk.role(),
            offset: chunk.offset(),
            length: u32::try_from(chunk.bytes().len()).map_err(invalid)?,
            hash: *blake3::hash(chunk.bytes()).as_bytes(),
        })
    }
}

/// Exact four-entry maximum grammar. Decoding it cannot recreate the original byte cursor,
/// producer barrier, accepted write, physical admission or Source/profile custody.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecordingDeclaredFinalizingInventory {
    version: u16,
    observation: RecordingFinalizingInventory,
    chunks: [Option<TerminalChunkDeclaration>; 4],
}
impl RecordingDeclaredFinalizingInventory {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn capture(
        proposal: &RecordingProposal,
        terminal: Uuid,
        acquisition: Uuid,
        generation: u64,
        settlements: &RecordingSettlements,
        barrier: &RecordingTerminalBarrier,
        normalizer: &RecordingPcmNormalizer,
    ) -> Result<(Self, RecordingPcmBatch), RecordingValueError> {
        let observation = RecordingFinalizingInventory::capture(
            proposal,
            terminal,
            acquisition,
            generation,
            settlements,
            barrier,
        )?;
        let batch =
            normalizer.prepare_original_terminal(proposal, terminal, settlements, barrier)?;
        let mut chunks = std::array::from_fn(|_| None);
        if batch.chunks().len() > chunks.len() {
            return Err(invalid("terminal declaration bound"));
        }
        for (slot, chunk) in chunks.iter_mut().zip(batch.chunks()) {
            *slot = Some(TerminalChunkDeclaration::from_chunk(chunk)?);
        }
        let value = Self {
            version: 3,
            observation,
            chunks,
        };
        value.validate(proposal)?;
        Ok((value, batch))
    }
    fn validate(&self, proposal: &RecordingProposal) -> Result<(), RecordingValueError> {
        self.observation.validate(proposal)?;
        if self.version != 3 {
            return Err(invalid("unsupported declared terminal version"));
        }
        let first = (self.observation.pending_offset - prefix::RECORDING_WAV_HEADER_BYTES as u64)
            / proposal.format().pcm_chunk_bytes();
        let offset =
            prefix::RECORDING_WAV_HEADER_BYTES as u64 + first * proposal.format().pcm_chunk_bytes();
        let remaining = self
            .observation
            .object_bytes
            .checked_sub(offset)
            .ok_or_else(|| invalid("terminal declaration extent"))?;
        let count = remaining.div_ceil(proposal.format().pcm_chunk_bytes()) as usize;
        if count > 2 {
            return Err(invalid("terminal declaration PCM bound"));
        }
        for index in 0..4 {
            if index >= count + 2 {
                if self.chunks[index].is_some() {
                    return Err(invalid("extra terminal declaration"));
                }
                continue;
            }
            let chunk = self.chunks[index]
                .as_ref()
                .ok_or_else(|| invalid("missing terminal declaration"))?;
            let (role, offset, length, header_hash) = if index < count {
                let ordinal = u32::try_from(first + index as u64).map_err(invalid)?;
                (
                    RecordingChunkRole::Pcm { ordinal },
                    offset + index as u64 * proposal.format().pcm_chunk_bytes(),
                    (remaining - index as u64 * proposal.format().pcm_chunk_bytes())
                        .min(proposal.format().pcm_chunk_bytes()) as u32,
                    None,
                )
            } else {
                match index - count {
                    0 => (
                        RecordingChunkRole::Header { revision: 1 },
                        4,
                        4,
                        Some(*blake3::hash(&self.observation.riff_length_patch).as_bytes()),
                    ),
                    1 => (
                        RecordingChunkRole::Header { revision: 2 },
                        64,
                        4,
                        Some(*blake3::hash(&self.observation.data_length_patch).as_bytes()),
                    ),
                    _ => unreachable!(),
                }
            };
            let hash = self.observation.reservation.proposal_hash;
            let id = super::super::pcm::terminal_chunk_identity(
                &hash,
                self.observation.terminal_id,
                role,
            );
            if chunk.id.is_nil()
                || chunk.id != id
                || chunk.role != role
                || chunk.offset != offset
                || chunk.length != length
                || header_hash.is_some_and(|hash| chunk.hash != hash)
            {
                return Err(invalid(
                    "terminal declaration role, identity, range or patch changed",
                ));
            }
        }
        Ok(())
    }
    pub(super) fn prefix_after_join(
        &self,
        proposal: &RecordingProposal,
        sequence: u64,
        settlements: &RecordingSettlements,
        joined: &reson::recording::RecordingJoinedCompletion,
    ) -> Result<prefix::RecordingPrefixInventory, RecordingValueError> {
        self.validate_joined(proposal, settlements, joined)?;
        self.observation
            .prefix_after_join(proposal, sequence, settlements, joined)
    }
    pub(super) fn validate_prefix_observation(
        &self,
        proposal: &RecordingProposal,
        prefix: &prefix::RecordingPrefixInventory,
    ) -> Result<(), RecordingValueError> {
        self.validate(proposal)?;
        if *prefix
            != self
                .observation
                .expected_prefix_observation(proposal, prefix.freeze_sequence())?
        {
            return Err(invalid(
                "Frozen accounting/terminal differs from original Finalizing observation",
            ));
        }
        Ok(())
    }
    /// Borrow the fixed declared identities; this inspection grants no retention or effect.
    pub(crate) fn inspect_pcm_chunk_ids<E>(
        &self,
        mut inspect: impl FnMut(Uuid) -> Result<(), E>,
    ) -> Result<(), E> {
        for chunk in self.chunks.iter().flatten() {
            if matches!(chunk.role, RecordingChunkRole::Pcm { .. }) {
                inspect(chunk.id)?;
            }
        }
        Ok(())
    }

    pub(crate) fn inspect_chunk_ids<E>(
        &self,
        mut visit: impl FnMut(Uuid) -> Result<(), E>,
    ) -> Result<(), E> {
        for chunk in self.chunks.iter().flatten() {
            visit(chunk.id)?;
        }
        Ok(())
    }
    pub(crate) fn validate_joined(
        &self,
        proposal: &RecordingProposal,
        settlements: &RecordingSettlements,
        joined: &reson::recording::RecordingJoinedCompletion,
    ) -> Result<(), RecordingValueError> {
        self.validate(proposal)?;
        self.observation
            .validate_joined(proposal, settlements, joined)
    }
    /// Only exact declared bytes match; this refusal check grants no write/admission permission.
    pub(crate) fn validate_chunk(
        &self,
        proposal: &RecordingProposal,
        chunk: &RecordingCapturedChunk,
    ) -> Result<(), RecordingValueError> {
        self.validate(proposal)?;
        chunk.validate_against(proposal)?;
        let observed = TerminalChunkDeclaration::from_chunk(chunk)?;
        if self
            .chunks
            .iter()
            .flatten()
            .any(|declared| declared == &observed)
        {
            Ok(())
        } else {
            Err(invalid("chunk differs from original terminal declaration"))
        }
    }
    pub(crate) fn encode(
        &self,
        proposal: &RecordingProposal,
    ) -> Result<Vec<u8>, RecordingValueError> {
        self.validate(proposal)?;
        let bytes = encode_value(self)?;
        if bytes.len() > CONTROL_BYTES {
            return Err(invalid("declared terminal control exceeds reservation"));
        }
        Ok(bytes)
    }
    pub(crate) fn decode(
        bytes: &[u8],
        proposal: &RecordingProposal,
    ) -> Result<Self, RecordingValueError> {
        if bytes.len() > CONTROL_BYTES {
            return Err(invalid("declared terminal control exceeds reservation"));
        }
        let value: Self = decode_value(bytes)?;
        value.validate(proposal)?;
        Ok(value)
    }
}
