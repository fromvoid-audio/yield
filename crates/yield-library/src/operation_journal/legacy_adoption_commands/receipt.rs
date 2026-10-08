//! Source-local immutable request/receipt bytes; parsing never restores migration authority.
use super::*;
use crate::operation_journal::OperationId;

pub(crate) const ADOPTION_METADATA_KEY: &str = "source.legacy_adoption.v1";
pub(crate) const MAX_ADOPTION_RECEIPT_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AdoptionReceiptEnvelope {
    version: u16,
    request: LegacyAdoptionIntentArgs,
    receipt: AdoptionReceipt,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct AdoptionReceipt {
    operation_id: OperationId,
    participant_id: ParticipantId,
    command_id: SourceCommandId,
    request_digest: [u8; 32],
    original_image_digest: [u8; 32],
    original_schema_digest: [u8; 32],
    current_schema_digest: [u8; 32],
    preserved_rows_digest: [u8; 32],
    metadata_rows: u64,
    audio_rows: u64,
    before_version: u64,
    after_version: u64,
    entered_synchronous: u64,
    entered_fullfsync: u64,
}

impl AdoptionReceiptEnvelope {
    pub(crate) fn capture(
        request: &LegacyAdoptionIntentArgs,
        operation_id: OperationId,
        original_schema_digest: [u8; 32],
        current_schema_digest: [u8; 32],
        preserved_rows_digest: [u8; 32],
        entered_synchronous: u64,
        entered_fullfsync: u64,
    ) -> Result<Self, LegacyAdoptionInputError> {
        request.validate()?;
        let request_digest = *blake3::hash(&request.canonical_bytes()?).as_bytes();
        let value = Self {
            version: 1,
            request: request.clone(),
            receipt: AdoptionReceipt {
                operation_id,
                participant_id: request.participant_id(),
                command_id: request.source_command_id(),
                request_digest,
                original_image_digest: *request.original_image_digest(),
                original_schema_digest,
                current_schema_digest,
                preserved_rows_digest,
                metadata_rows: request.data.metadata_rows,
                audio_rows: request.data.audio_rows,
                before_version: request.data.legacy_version,
                after_version: request.data.target_version,
                entered_synchronous,
                entered_fullfsync,
            },
        };
        value.validate()?;
        Ok(value)
    }
    fn validate(&self) -> Result<(), LegacyAdoptionInputError> {
        self.request.validate()?;
        let receipt = &self.receipt;
        if self.version != 1
            || receipt.operation_id.as_uuid().is_nil()
            || receipt.participant_id != self.request.participant_id()
            || receipt.command_id != self.request.source_command_id()
            || receipt.request_digest != *blake3::hash(&self.request.canonical_bytes()?).as_bytes()
            || receipt.original_image_digest != *self.request.original_image_digest()
            || receipt.metadata_rows != self.request.data.metadata_rows
            || receipt.audio_rows != self.request.data.audio_rows
            || receipt.before_version != 11
            || receipt.after_version != 13
            || receipt.entered_synchronous != 3
            || receipt.entered_fullfsync > 1
        {
            return Err(LegacyAdoptionInputError::Invalid);
        }
        Ok(())
    }
    pub(crate) fn encode(&self) -> Result<Vec<u8>, LegacyAdoptionInputError> {
        self.validate()?;
        let encoded = serde_json::to_vec(self).map_err(|_| LegacyAdoptionInputError::Wire)?;
        if encoded.is_empty() || encoded.len() > MAX_ADOPTION_RECEIPT_BYTES {
            return Err(LegacyAdoptionInputError::Wire);
        }
        Ok(encoded)
    }
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, LegacyAdoptionInputError> {
        if bytes.is_empty() || bytes.len() > MAX_ADOPTION_RECEIPT_BYTES {
            return Err(LegacyAdoptionInputError::Wire);
        }
        let value: Self =
            serde_json::from_slice(bytes).map_err(|_| LegacyAdoptionInputError::Wire)?;
        value.validate()?;
        if value.encode()? != bytes {
            return Err(LegacyAdoptionInputError::Wire);
        }
        Ok(value)
    }
    pub(crate) fn historical_request(&self) -> &LegacyAdoptionIntentArgs {
        &self.request
    }
    pub(crate) fn historical_operation(&self) -> OperationId {
        self.receipt.operation_id
    }
    pub(crate) fn matches_preserved_rows(&self, rows: [u8; 32]) -> bool {
        self.receipt.preserved_rows_digest == rows
    }
    // Schema allowlisting only; this is never an adoption/claim/current-owner proof.
    pub(crate) fn matches_migration_schema(&self, schema: [u8; 32]) -> bool {
        self.receipt.current_schema_digest == schema
    }
    pub(crate) fn matches_postimage(&self, schema: [u8; 32], rows: [u8; 32]) -> bool {
        self.receipt.current_schema_digest == schema && self.receipt.preserved_rows_digest == rows
    }
    pub(crate) fn matches_original(
        &self,
        request: &LegacyAdoptionIntentArgs,
        operation: OperationId,
    ) -> bool {
        self.request == *request && self.receipt.operation_id == operation
    }
}
