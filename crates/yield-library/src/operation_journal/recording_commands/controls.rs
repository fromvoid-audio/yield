//! Reserved durable metadata observations, never a terminal capability or release permission.
use super::*;

pub(crate) const CONTROL_BYTES: usize = 4096;
pub(crate) const CONTROL_ROWS: i64 = 4;
pub(crate) const RESERVED_BYTES: u64 = CONTROL_ROWS as u64 * CONTROL_BYTES as u64;
pub(crate) const FIRST_CONTROL_SLOT: i64 = 20_000;
const VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum ControlRole {
    Finalizing,
    Frozen,
    Seal,
    Retirement,
}
impl ControlRole {
    pub(crate) const ALL: [Self; 4] =
        [Self::Finalizing, Self::Frozen, Self::Seal, Self::Retirement];
    pub(crate) fn slot(self) -> i64 {
        FIRST_CONTROL_SLOT
            + match self {
                Self::Finalizing => 0,
                Self::Frozen => 1,
                Self::Seal => 2,
                Self::Retirement => 3,
            }
    }
}

/// Only the empty reservation version is understood here. Future terminal values must be
/// explicitly validated by the actual closure owner; they cannot be treated as empty slots.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecordingControlReservation {
    version: u16,
    operation_id: OperationId,
    proposal_hash: [u8; 32],
    output_id: Uuid,
    work_key: WorkUnitKey,
    role: ControlRole,
    control_id: Uuid,
}

pub(crate) fn reservation_metadata_bound(
    proposal: &RecordingProposal,
) -> Result<u64, RecordingValueError> {
    proposal.validate()?;
    if usize::from(proposal.format.header_bytes) != prefix::RECORDING_WAV_HEADER_BYTES
        || proposal.format.pcm_slots()? > prefix::MAX_SEALED_RECORDING_PCM_CHUNKS
    {
        return Err(invalid(
            "historical recording proposal is unsupported for new capture",
        ));
    }
    proposal
        .retained_metadata_bound()?
        .checked_add(RESERVED_BYTES)
        .filter(|n| *n <= MAX_RECORDING_RETAINED_ENCODED_BYTES)
        .ok_or_else(|| invalid("recording inventory leaves no reserved terminal metadata budget"))
}

impl RecordingControlReservation {
    pub(crate) fn new(
        proposal: &RecordingProposal,
        role: ControlRole,
    ) -> Result<Self, RecordingValueError> {
        reservation_metadata_bound(proposal)?;
        let proposal_hash = *blake3::hash(&proposal.encode()?).as_bytes();
        let mut hash = blake3::Hasher::new();
        hash.update(b"yield.recording.control.reservation.v1\0");
        hash.update(&proposal_hash);
        hash.update(&role.slot().to_le_bytes());
        let mut key = [0; 16];
        key.copy_from_slice(&hash.finalize().as_bytes()[..16]);
        let control_id = Uuid::from_bytes(key);
        if control_id.is_nil() {
            return Err(invalid("nil recording control identity"));
        }
        Ok(Self {
            version: VERSION,
            operation_id: proposal.operation_id,
            proposal_hash,
            output_id: proposal.output_id,
            work_key: proposal.work_key,
            role,
            control_id,
        })
    }
    pub(crate) fn id(&self) -> Uuid {
        self.control_id
    }
    pub(crate) fn slot(&self) -> i64 {
        self.role.slot()
    }
    pub(crate) fn encode(
        &self,
        proposal: &RecordingProposal,
    ) -> Result<Vec<u8>, RecordingValueError> {
        if self != &Self::new(proposal, self.role)? {
            return Err(invalid("conflicting recording control reservation"));
        }
        let bytes = encode_value(self)?;
        if bytes.len() > CONTROL_BYTES {
            return Err(invalid("recording control exceeds reserved bound"));
        }
        Ok(bytes)
    }
    pub(crate) fn decode(
        bytes: &[u8],
        proposal: &RecordingProposal,
    ) -> Result<Self, RecordingValueError> {
        if bytes.len() > CONTROL_BYTES {
            return Err(invalid("recording control exceeds reserved bound"));
        }
        let value: Self = decode_value(bytes)?;
        value.encode(proposal)?;
        Ok(value)
    }
}

#[cfg(test)]
mod tests;

// Actual owner integration consumes the original private producer barrier; never a decoded grant.
mod terminal;
pub(crate) use terminal::RecordingFinalizingInventory;
