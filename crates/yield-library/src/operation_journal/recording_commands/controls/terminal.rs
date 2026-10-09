//! Bounded original producer inventory observations; decoding never restores a close capability.
use super::*;
use crate::operation_journal::recording_commands::prefix::RecordingTerminalKind;
use reson::recording::{
    RecordingJoinedCompletion, RecordingPrefixState, RecordingSettlements, RecordingTerminalBarrier,
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecordingFinalizingInventory {
    version: u16,
    pub(super) reservation: RecordingControlReservation,
    pub(super) terminal_id: Uuid,
    acquisition: Uuid,
    generation: u64,
    terminal: RecordingTerminalKind,
    frames: u64,
    accepted: u64,
    encoded: u64,
    abandoned: u64,
    unconfirmed: u64,
    rejected: u64,
    overruns: u64,
    pending_write_id: Uuid,
    riff_write_id: Uuid,
    data_write_id: Uuid,
    pub(super) pending_offset: u64,
    pending_length: u32,
    pending_hash: [u8; 32],
    pub(super) object_bytes: u64,
    pub(super) riff_length_patch: [u8; 4],
    pub(super) data_length_patch: [u8; 4],
}

impl RecordingFinalizingInventory {
    pub(crate) fn terminal_id(&self) -> Uuid {
        self.terminal_id
    }
    pub(super) fn prefix_after_join(
        &self,
        proposal: &RecordingProposal,
        sequence: u64,
        settlements: &RecordingSettlements,
        joined: &RecordingJoinedCompletion,
    ) -> Result<prefix::RecordingPrefixInventory, RecordingValueError> {
        self.validate_joined(proposal, settlements, joined)?;
        self.expected_prefix_observation(proposal, sequence)
    }
    /// Compare historical metadata only; this cannot authenticate successful producer join.
    pub(super) fn expected_prefix_observation(
        &self,
        proposal: &RecordingProposal,
        sequence: u64,
    ) -> Result<prefix::RecordingPrefixInventory, RecordingValueError> {
        self.validate(proposal)?;
        prefix::RecordingPrefixInventory::new(
            proposal,
            self.terminal_id,
            sequence,
            self.generation,
            2,
            self.terminal,
            prefix::RecordingSampleAccounting {
                accepted: self.accepted,
                encoded: self.encoded,
                abandoned: self.abandoned,
                unconfirmed: self.unconfirmed,
                rejected: self.rejected,
                overruns: self.overruns,
                complete: true,
                overflowed: false,
                joined_successfully: true,
                writer_failed: false,
            },
        )
    }
    /// Capture original private producer evidence; this data alone authorizes no transaction.
    pub(crate) fn capture(
        proposal: &RecordingProposal,
        terminal_id: Uuid,
        acquisition: Uuid,
        generation: u64,
        settlements: &RecordingSettlements,
        barrier: &RecordingTerminalBarrier,
    ) -> Result<Self, RecordingValueError> {
        if !settlements.owns_barrier(barrier) {
            return Err(invalid("terminal barrier belongs to a different producer"));
        }
        let format = proposal.format();
        let limits = barrier.limits();
        if limits.sample_rate() != format.sample_rate()
            || limits.channels() != format.channels()
            || limits.max_frames() != format.max_frames()
        {
            return Err(invalid("original terminal producer format changed"));
        }
        let accounting = barrier.accounting();
        let writes = barrier.write_inventory();
        let value = Self {
            version: 2,
            reservation: RecordingControlReservation::new(proposal, ControlRole::Finalizing)?,
            terminal_id,
            acquisition,
            generation,
            terminal: match barrier.state() {
                RecordingPrefixState::StoppedUnpublished => RecordingTerminalKind::Stop,
                RecordingPrefixState::CancelledRetained => RecordingTerminalKind::Cancel,
            },
            frames: barrier.frames(),
            accepted: accounting.accepted_samples,
            encoded: accounting.encoded_samples,
            abandoned: accounting.abandoned_samples,
            unconfirmed: accounting.unconfirmed_samples,
            rejected: accounting.dropped_samples_at_barrier,
            overruns: accounting.overrun_events_at_barrier,
            pending_write_id: terminal_write_id(proposal, terminal_id, 0)?,
            riff_write_id: terminal_write_id(proposal, terminal_id, 1)?,
            data_write_id: terminal_write_id(proposal, terminal_id, 2)?,
            pending_offset: writes.pending_offset(),
            pending_length: u32::try_from(writes.pending_bytes().len()).map_err(invalid)?,
            pending_hash: *blake3::hash(writes.pending_bytes()).as_bytes(),
            object_bytes: writes.object_bytes(),
            riff_length_patch: *writes.riff_length_patch(),
            data_length_patch: *writes.data_length_patch(),
        };
        value.validate(proposal)?;
        Ok(value)
    }
    pub(super) fn validate(&self, proposal: &RecordingProposal) -> Result<(), RecordingValueError> {
        if self.version != 2
            || self.reservation
                != RecordingControlReservation::new(proposal, ControlRole::Finalizing)?
            || self.terminal_id.is_nil()
            || self.acquisition.is_nil()
            || self.generation == 0
            || self.pending_write_id != terminal_write_id(proposal, self.terminal_id, 0)?
            || self.riff_write_id != terminal_write_id(proposal, self.terminal_id, 1)?
            || self.data_write_id != terminal_write_id(proposal, self.terminal_id, 2)?
            || [
                self.accepted,
                self.encoded,
                self.abandoned,
                self.unconfirmed,
                self.rejected,
                self.overruns,
            ]
            .contains(&u64::MAX)
            || self.unconfirmed != 0
            || self
                .encoded
                .checked_add(self.abandoned)
                .and_then(|n| n.checked_add(self.unconfirmed))
                != Some(self.accepted)
            || (self.terminal == RecordingTerminalKind::Stop && self.abandoned != 0)
            || [self.accepted, self.encoded, self.abandoned]
                .into_iter()
                .any(|n| n % u64::from(proposal.format().channels()) != 0)
            || self.frames != self.encoded / u64::from(proposal.format().channels())
            || self.frames > proposal.format().max_frames()
            || self
                .encoded
                .checked_mul(4)
                .and_then(|n| n.checked_add(prefix::RECORDING_WAV_HEADER_BYTES as u64))
                != Some(self.object_bytes)
            || self.pending_offset < prefix::RECORDING_WAV_HEADER_BYTES as u64
            || self.pending_length > 16 * 1024
            || self
                .pending_offset
                .checked_add(u64::from(self.pending_length))
                != Some(self.object_bytes)
            || u64::from(u32::from_le_bytes(self.riff_length_patch))
                != self.object_bytes.checked_sub(8).unwrap_or(u64::MAX)
            || u64::from(u32::from_le_bytes(self.data_length_patch))
                != self
                    .object_bytes
                    .checked_sub(prefix::RECORDING_WAV_HEADER_BYTES as u64)
                    .unwrap_or(u64::MAX)
        {
            return Err(invalid(
                "original terminal inventory is invalid or conflicting",
            ));
        }
        Ok(())
    }
    pub(crate) fn encode(
        &self,
        proposal: &RecordingProposal,
    ) -> Result<Vec<u8>, RecordingValueError> {
        self.validate(proposal)?;
        let encoded = encode_value(self)?;
        if encoded.len() > CONTROL_BYTES {
            return Err(invalid("terminal inventory exceeds reserved control bound"));
        }
        Ok(encoded)
    }
    pub(crate) fn decode(
        bytes: &[u8],
        proposal: &RecordingProposal,
    ) -> Result<Self, RecordingValueError> {
        if bytes.is_empty() || bytes.len() > CONTROL_BYTES {
            return Err(invalid("terminal inventory exceeds reserved control bound"));
        }
        let value: Self = decode_value(bytes)?;
        value.validate(proposal)?;
        Ok(value)
    }
    /// Genuine successful join must preserve the exact pre-finalize facts and bytes.
    pub(crate) fn validate_joined(
        &self,
        proposal: &RecordingProposal,
        settlements: &RecordingSettlements,
        joined: &RecordingJoinedCompletion,
    ) -> Result<(), RecordingValueError> {
        self.validate(proposal)?;
        if !settlements.owns_completion(joined) {
            return Err(invalid("joined completion belongs to a different producer"));
        }
        let format = proposal.format();
        let limits = joined.limits();
        let accounting = joined.accounting();
        let writes = joined.write_inventory();
        let terminal = match joined.state() {
            RecordingPrefixState::StoppedUnpublished => RecordingTerminalKind::Stop,
            RecordingPrefixState::CancelledRetained => RecordingTerminalKind::Cancel,
        };
        if limits.sample_rate() != format.sample_rate()
            || limits.channels() != format.channels()
            || limits.max_frames() != format.max_frames()
            || joined.frames() != self.frames
            || terminal != self.terminal
            || accounting.accepted_samples != self.accepted
            || accounting.encoded_samples != self.encoded
            || accounting.abandoned_samples != self.abandoned
            || accounting.unconfirmed_samples != self.unconfirmed
            || accounting.dropped_samples_at_barrier != self.rejected
            || accounting.overrun_events_at_barrier != self.overruns
            || writes.pending_offset() != self.pending_offset
            || writes.pending_bytes().len() as u64 != u64::from(self.pending_length)
            || *blake3::hash(writes.pending_bytes()).as_bytes() != self.pending_hash
            || writes.object_bytes() != self.object_bytes
            || writes.riff_length_patch() != &self.riff_length_patch
            || writes.data_length_patch() != &self.data_length_patch
        {
            return Err(invalid(
                "successful join differs from original frozen terminal inventory",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;

fn terminal_write_id(
    proposal: &RecordingProposal,
    terminal: Uuid,
    role: u8,
) -> Result<Uuid, RecordingValueError> {
    let mut hash = blake3::Hasher::new();
    hash.update(b"yield.recording.terminal.write.v2\0");
    hash.update(blake3::hash(&proposal.encode()?).as_bytes());
    hash.update(terminal.as_bytes());
    hash.update(&[role]);
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&hash.finalize().as_bytes()[..16]);
    let id = Uuid::from_bytes(bytes);
    if id.is_nil() {
        return Err(invalid("nil terminal write identity"));
    }
    Ok(id)
}
