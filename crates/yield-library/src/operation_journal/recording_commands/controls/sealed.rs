//! Immutable retained-byte seal data. Decoding supplies no verification or physical grant.
use super::*;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecordingSealedInventory {
    version: u16,
    reservation: RecordingControlReservation,
    frozen_hash: [u8; 32],
    seal: prefix::RecordingPrefixSeal,
}
impl RecordingSealedInventory {
    pub(crate) fn capture(
        proposal: &RecordingProposal,
        frozen: &RecordingFrozenInventory,
        seal: &prefix::RecordingPrefixSeal,
    ) -> Result<Self, RecordingValueError> {
        let value = Self {
            version: 5,
            reservation: RecordingControlReservation::new(proposal, ControlRole::Seal)?,
            frozen_hash: *blake3::hash(&frozen.encode(proposal)?).as_bytes(),
            seal: seal.clone(),
        };
        value.validate_frozen(proposal, frozen)?;
        Ok(value)
    }
    fn validate(&self, proposal: &RecordingProposal) -> Result<(), RecordingValueError> {
        if self.version != 5
            || self.frozen_hash == [0; 32]
            || self.reservation != RecordingControlReservation::new(proposal, ControlRole::Seal)?
        {
            return Err(invalid("conflicting Seal reservation/version"));
        }
        self.seal.encode(proposal)?;
        Ok(())
    }
    pub(crate) fn validate_frozen(
        &self,
        proposal: &RecordingProposal,
        frozen: &RecordingFrozenInventory,
    ) -> Result<(), RecordingValueError> {
        self.validate(proposal)?;
        if self.seal.inventory() != frozen.prefix()
            || self.frozen_hash != *blake3::hash(&frozen.encode(proposal)?).as_bytes()
        {
            return Err(invalid("Seal differs from original Frozen inventory"));
        }
        Ok(())
    }
    pub(crate) fn encode(
        &self,
        proposal: &RecordingProposal,
    ) -> Result<Vec<u8>, RecordingValueError> {
        self.validate(proposal)?;
        let bytes = encode_value(self)?;
        if bytes.len() > CONTROL_BYTES {
            return Err(invalid("Seal exceeds reserved control bound"));
        }
        Ok(bytes)
    }
    pub(crate) fn decode(
        bytes: &[u8],
        proposal: &RecordingProposal,
    ) -> Result<Self, RecordingValueError> {
        if bytes.len() > CONTROL_BYTES {
            return Err(invalid("Seal exceeds reserved control bound"));
        }
        let value: Self = decode_value(bytes)?;
        value.validate(proposal)?;
        Ok(value)
    }
}
