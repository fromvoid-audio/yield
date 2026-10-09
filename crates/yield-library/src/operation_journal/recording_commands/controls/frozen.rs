//! Known immutable Frozen data; decoding supplies no live producer/Source capability.
use super::*;
use reson::recording::{RecordingJoinedCompletion, RecordingSettlements};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecordingFrozenInventory {
    version: u16,
    reservation: RecordingControlReservation,
    finalizing_hash: [u8; 32],
    prefix: prefix::RecordingPrefixInventory,
}
impl RecordingFrozenInventory {
    /// Called only with the original joined evidence. Store retention must additionally
    /// validate every declared admitted byte row and its original Source context.
    pub(crate) fn capture(
        proposal: &RecordingProposal,
        finalizing: &RecordingDeclaredFinalizingInventory,
        sequence: u64,
        settlements: &RecordingSettlements,
        joined: &RecordingJoinedCompletion,
    ) -> Result<Self, RecordingValueError> {
        let value = Self {
            version: 4,
            reservation: RecordingControlReservation::new(proposal, ControlRole::Frozen)?,
            finalizing_hash: *blake3::hash(&finalizing.encode(proposal)?).as_bytes(),
            prefix: finalizing.prefix_after_join(proposal, sequence, settlements, joined)?,
        };
        value.validate(proposal)?;
        Ok(value)
    }
    fn validate(&self, proposal: &RecordingProposal) -> Result<(), RecordingValueError> {
        if self.version != 4
            || self.finalizing_hash == [0; 32]
            || self.reservation != RecordingControlReservation::new(proposal, ControlRole::Frozen)?
        {
            return Err(invalid("conflicting Frozen reservation/version"));
        }
        self.prefix().encode(proposal)?;
        Ok(())
    }
    pub(crate) fn validate_finalizing(
        &self,
        proposal: &RecordingProposal,
        finalizing: &RecordingDeclaredFinalizingInventory,
    ) -> Result<(), RecordingValueError> {
        self.validate(proposal)?;
        finalizing.validate_prefix_observation(proposal, &self.prefix)?;
        if self.finalizing_hash != *blake3::hash(&finalizing.encode(proposal)?).as_bytes() {
            return Err(invalid(
                "Frozen declaration differs from original Finalizing inventory",
            ));
        }
        Ok(())
    }
    pub(crate) fn prefix(&self) -> &prefix::RecordingPrefixInventory {
        &self.prefix
    }
    pub(crate) fn encode(
        &self,
        proposal: &RecordingProposal,
    ) -> Result<Vec<u8>, RecordingValueError> {
        self.validate(proposal)?;
        let bytes = encode_value(self)?;
        if bytes.len() > CONTROL_BYTES {
            return Err(invalid("Frozen control exceeds original reservation"));
        }
        Ok(bytes)
    }
    pub(crate) fn decode(
        bytes: &[u8],
        proposal: &RecordingProposal,
    ) -> Result<Self, RecordingValueError> {
        if bytes.len() > CONTROL_BYTES {
            return Err(invalid("Frozen control exceeds original reservation"));
        }
        let value: Self = decode_value(bytes)?;
        value.validate(proposal)?;
        Ok(value)
    }
}
