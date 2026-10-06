//! Immutable publication issuance observations. No decoder can reconstruct a live grant.
use super::staging_checkpoints::StagingCheckpointRecord;
use super::{ChunkClaimState, ChunkId, OperationJournalStore, OperationJournalStoreError};
use crate::operation_journal::admission_claims::AdmissionClaim;
use crate::operation_journal::full_capacity::FullCapacityAllocationClass;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub(super) const TABLE_NAME: &str = "operation_journal_publication_issuance";
pub(super) const CREATE_TABLE: &str = "CREATE TABLE operation_journal_publication_issuance (
    output_id TEXT NOT NULL PRIMARY KEY,
    publication_effect_id TEXT NOT NULL UNIQUE,
    record BLOB NOT NULL CHECK(typeof(record) = 'blob' AND length(record) BETWEEN 1 AND 16384)
)";
pub(super) const MAX_RECORD_BYTES: usize = 16 * 1024;
const MAX_COMMAND_BYTES: usize = 12 * 1024;
const MAX_ROWS: i64 = 8192;

pub(super) fn create(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(CREATE_TABLE)
}
fn invalid(reason: impl Into<String>) -> OperationJournalStoreError {
    OperationJournalStoreError::StagingCheckpoint {
        reason: reason.into(),
    }
}

fn ordinary_read_error(
    error: super::ordinary_claims::OrdinaryClaimReadError,
) -> OperationJournalStoreError {
    match error {
        super::ordinary_claims::OrdinaryClaimReadError::Sql(error) => error.into(),
        other => invalid(format!("publication charge readback: {other}")),
    }
}
fn chunk_read_error(error: super::chunk_claims::ChunkClaimReadError) -> OperationJournalStoreError {
    match error {
        super::chunk_claims::ChunkClaimReadError::Sql(error) => error.into(),
        other => invalid(format!("publication chunk readback: {other}")),
    }
}

mod observations;
use observations::{
    AuthorityObservation, CommandObservation, ExpectedObservation, PublicationObservation,
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ChunkObservation {
    chunk_id: Uuid,
    issue_id: Uuid,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublicationBindingDraft {
    schema_version: u32,
    output_id: Uuid,
    checkpoint_sequence: i64,
    staging_effect_id: Uuid,
    staging_fingerprint: [u8; 32],
    issuer_incarnation: Uuid,
    slot_generation: u64,
    command: CommandObservation,
    command_fingerprint: [u8; 32],
    charge: AdmissionClaim,
    charge_generation: u64,
    charge_incremental_bytes: u64,
    chunk: Option<ChunkObservation>,
}
// Header + at most three source-fence string groups and two1024-byte locators
// fit8KiB per typed projection, including the single source Vec element.
const _: () = assert!(std::mem::size_of::<PublicationBindingDraft>() <= 2048);
const _: () =
    assert!(std::mem::size_of::<super::staging_checkpoints::StoredStagingSourceFence>() <= 256);
impl PublicationBindingDraft {
    pub(super) fn capture(
        canonical_command: &[u8],
        staging: &StagingCheckpointRecord,
        charge: &AdmissionClaim,
        charge_observation: (u64, u64),
        chunk: Option<(Uuid, Uuid)>,
        issuer: (Uuid, u64),
    ) -> Result<Self, OperationJournalStoreError> {
        if canonical_command.len() > MAX_COMMAND_BYTES {
            return Err(invalid("publication command exceeds durable binding bound"));
        }
        super::staging_checkpoints::bounded_string_tokens(canonical_command).map_err(invalid)?;
        // Direct typed decoding rejects duplicate/unknown fields without an owned JSON tree.
        let command = serde_json::from_slice(canonical_command)
            .map_err(|_| invalid("original publication command shape is invalid"))?;
        let record = Self {
            schema_version: 1,
            output_id: staging.output_id(),
            checkpoint_sequence: 1,
            staging_effect_id: staging.effect_id(),
            staging_fingerprint: staging.request_fingerprint(),
            issuer_incarnation: issuer.0,
            slot_generation: issuer.1,
            command,
            command_fingerprint: *blake3::hash(canonical_command).as_bytes(),
            charge: charge.clone(),
            charge_generation: charge_observation.0,
            charge_incremental_bytes: charge_observation.1,
            chunk: chunk.map(|(chunk_id, issue_id)| ChunkObservation { chunk_id, issue_id }),
        };
        record.validate_staging(staging)?;
        Ok(record)
    }
    fn validate_staging(
        &self,
        staging: &StagingCheckpointRecord,
    ) -> Result<(), OperationJournalStoreError> {
        let PublicationObservation::PublishNoReplace {
            staging: stage,
            target,
            staged,
            expected_absent,
        } = &self.command.effect;
        stage.binding.validate().map_err(invalid)?;
        target.binding.validate().map_err(invalid)?;
        self.command
            .target_registration
            .validate()
            .map_err(invalid)?;
        self.charge
            .validate()
            .map_err(|_| invalid("publication charge is invalid"))?;
        for source in &self.command.source_fences {
            source.validate().map_err(invalid)?;
        }
        let sources = staging
            .source_fence()
            .map(std::slice::from_ref)
            .unwrap_or(&[]);
        if self.schema_version != 1
            || self.output_id != staging.output_id()
            || self.checkpoint_sequence <= 0
            || self.issuer_incarnation.is_nil()
            || self.slot_generation == 0
            || self.staging_effect_id != staging.effect_id()
            || self.staging_fingerprint != staging.request_fingerprint()
            || self.command.operation_id != staging.operation_id()
            || self.command.participant_id != staging.participant_id()
            || self.command.effect_id.is_nil()
            || self.command.effect_id == self.staging_effect_id
            || self.command.source_fences.as_slice() != sources
            || self.command.expected != ExpectedObservation::Absent
            || stage.binding.registration != *staging.staging_root()
            || stage.binding.locator != staging.locator()
            || stage.binding.expected != ExpectedObservation::Exact(staging.logical_object_id())
            || staged.object != staging.logical_object_id()
            || staged.bytes != staging.bytes()
            || target.binding.registration != self.command.target_registration
            || target.binding.registration.owner() != stage.binding.registration.owner()
            || target.binding.expected != ExpectedObservation::Absent
            || expected_absent.marker != 1
            || self.charge.allocation_class() != FullCapacityAllocationClass::DestinationStaging
            || staged.bytes == 0
            || self.charge.allocated_bytes() < staged.bytes
            || self.charge_generation == 0
            || self
                .chunk
                .as_ref()
                .is_some_and(|chunk| chunk.chunk_id.is_nil() || chunk.issue_id.is_nil())
        {
            return Err(invalid(
                "publication binding differs from exact original staging/charge",
            ));
        }
        for root in [&stage.binding, &target.binding] {
            if let AuthorityObservation::Source(source) = &root.authority
                && !sources.contains(source)
            {
                return Err(invalid(
                    "publication root authority differs from original source fence",
                ));
            }
        }
        let mut hash = CanonicalHash {
            hash: blake3::Hasher::new(),
            length: 0,
        };
        serde_json::to_writer(&mut hash, &self.command)
            .map_err(|_| invalid("publication command canonical shape exceeds bound"))?;
        if self.command_fingerprint != *hash.hash.finalize().as_bytes() {
            return Err(invalid(
                "publication command fingerprint differs from retained fields",
            ));
        }
        Ok(())
    }
    pub(super) fn finish(&mut self, sequence: i64) {
        self.checkpoint_sequence = sequence;
    }
    pub(super) fn publication_effect(&self) -> Uuid {
        self.command.effect_id
    }
    #[cfg(test)]
    pub(super) fn canonical_command_fixture(value: serde_json::Value) -> Vec<u8> {
        let command: CommandObservation =
            serde_json::from_value(value).expect("data-only command fixture");
        serde_json::to_vec(&command).expect("data-only canonical command fixture")
    }
    pub(super) fn encode(&self) -> Result<Vec<u8>, OperationJournalStoreError> {
        let encoded =
            serde_json::to_vec(self).map_err(|_| invalid("publication binding encode failed"))?;
        if encoded.len() > MAX_RECORD_BYTES {
            return Err(invalid("publication binding exceeds record bound"));
        }
        Ok(encoded)
    }
    fn decode(bytes: &[u8]) -> Result<Self, OperationJournalStoreError> {
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(invalid("publication binding exceeds record bound"));
        }
        super::staging_checkpoints::bounded_string_tokens(bytes).map_err(invalid)?;
        let record: Self = serde_json::from_slice(bytes)
            .map_err(|_| invalid("publication binding has malformed or duplicate fields"))?;
        if record.encode()? != bytes {
            return Err(invalid("publication binding is not canonical"));
        }
        Ok(record)
    }
}

struct CanonicalHash {
    hash: blake3::Hasher,
    length: usize,
}
impl std::io::Write for CanonicalHash {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let after = self
            .length
            .checked_add(bytes.len())
            .filter(|n| *n <= MAX_COMMAND_BYTES)
            .ok_or_else(|| std::io::Error::other("publication canonical command bound"))?;
        self.hash.update(bytes);
        self.length = after;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl OperationJournalStore {
    pub(crate) fn capture_publication_binding(
        &self,
        capture: PublicationBindingCapture<'_>,
    ) -> Result<PublicationBindingDraft, OperationJournalStoreError> {
        self.validate_namespace()?;
        let staging =
            StagingCheckpointRecord::from_file_result(capture.validation).map_err(invalid)?;
        let observation = capture
            .charge
            .with_current(|observation| observation)
            .ok_or_else(|| invalid("original publication charge generation is not current"))?;
        let chunk = if let Some(id) = capture.chunk {
            let retained = self
                .read_chunk_claim(id)?
                .ok_or_else(|| invalid("publication selected chunk is absent"))?;
            let issue = retained
                .issue_id()
                .filter(|_| retained.state() == ChunkClaimState::Issued)
                .ok_or_else(|| invalid("publication selected chunk is not issued"))?;
            Some((id.as_uuid(), issue))
        } else {
            None
        };
        PublicationBindingDraft::capture(
            capture.canonical_command,
            &staging,
            capture.charge.claim(),
            (observation.generation, observation.incremental_bytes),
            chunk,
            capture.issuer,
        )
    }
    pub(super) fn validate_publication_charge_record(
        &self,
        binding: &PublicationBindingDraft,
    ) -> Result<(), OperationJournalStoreError> {
        let ordinary =
            super::ordinary_claims::read_claim(&self.connection, binding.charge.claim_id())
                .map_err(ordinary_read_error)?
                .ok_or_else(|| invalid("publication ordinary charge is absent"))?;
        let released =
            super::ordinary_claims::read_release(&self.connection, binding.charge.claim_id())
                .map_err(ordinary_read_error)?
                .is_some();
        if released
            || ordinary.operation_id() != binding.command.operation_id
            || ordinary.volume() != binding.charge.volume()
            || ordinary.allocation_class() != binding.charge.allocation_class()
            || ordinary.allocated_bytes() != binding.charge.allocated_bytes()
        {
            return Err(invalid(
                "publication selected charge conflicts or is released",
            ));
        }
        if let Some(selected) = &binding.chunk {
            let chunk_id = ChunkId::from_uuid(selected.chunk_id)
                .map_err(|_| invalid("publication selected chunk is invalid"))?;
            let chunk = super::chunk_claims::read(&self.connection, chunk_id)
                .map_err(chunk_read_error)?
                .ok_or_else(|| invalid("publication selected chunk is absent"))?;
            if chunk.operation_id() != binding.command.operation_id
                || chunk.participant_id() != binding.command.participant_id
                || chunk.claim_id() != binding.charge.claim_id()
                || chunk.volume() != binding.charge.volume()
                || chunk.allocation_class() != binding.charge.allocation_class()
                || chunk.allocated_bytes() != binding.charge.allocated_bytes()
                || chunk.state() != ChunkClaimState::Issued
                || chunk.issue_id() != Some(selected.issue_id)
            {
                return Err(invalid(
                    "publication selected chunk conflicts or is no longer issued",
                ));
            }
        }
        Ok(())
    }
    pub(super) fn insert_publication_binding_inside_transaction(
        &self,
        staging: &StagingCheckpointRecord,
        binding: &PublicationBindingDraft,
    ) -> Result<(), OperationJournalStoreError> {
        binding.validate_staging(staging)?;
        self.validate_publication_charge_record(binding)?;
        let count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM (SELECT 1 FROM operation_journal_publication_issuance LIMIT 8192)",
            [],
            |row| row.get(0),
        )?;
        if !(0..MAX_ROWS).contains(&count) {
            return Err(invalid("publication issuance row bound exhausted"));
        }
        let encoded = binding.encode()?;
        self.connection.execute(
            "INSERT INTO operation_journal_publication_issuance(output_id, publication_effect_id, record) VALUES (?1, ?2, ?3)",
            params![binding.output_id.to_string(), binding.publication_effect().to_string(), encoded])?;
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn publication_binding_observation_for_test(
        &self,
        output: Uuid,
    ) -> serde_json::Value {
        let binding = self
            .read_publication_binding(output)
            .expect("exact durable original binding readback")
            .expect("retained issuance binding");
        serde_json::to_value(binding).expect("bounded data-only issuance observation")
    }
    pub(super) fn read_publication_binding(
        &self,
        output: Uuid,
    ) -> Result<Option<PublicationBindingDraft>, OperationJournalStoreError> {
        let record = self.connection.query_row(
            "SELECT output_id, publication_effect_id, record FROM operation_journal_publication_issuance WHERE output_id = ?1",
            params![output.to_string()], |row| {
                use rusqlite::types::ValueRef;
                let ValueRef::Blob(bytes) = row.get_ref(2)? else {
                    return Ok(Err(invalid("publication binding record is not a blob")));
                };
                let decoded = PublicationBindingDraft::decode(bytes);
                let Ok(binding) = decoded else { return Ok(decoded); };
                if row.get_ref(0)? != ValueRef::Text(output.to_string().as_bytes())
                    || row.get_ref(1)? != ValueRef::Text(binding.publication_effect().to_string().as_bytes())
                    || binding.output_id != output {
                    return Ok(Err(invalid("publication binding keys differ from exact record")));
                }
                Ok(Ok(binding))
            }).optional()?.transpose()?;
        let Some(binding) = record else {
            return Ok(None);
        };
        let (sequence, staging, reserved) = self
            .read_staging_checkpoint_row(output)?
            .ok_or_else(|| invalid("publication binding has no original staging checkpoint"))?;
        if !reserved || sequence != binding.checkpoint_sequence {
            return Err(invalid(
                "publication binding checkpoint is not exactly reserved",
            ));
        }
        binding.validate_staging(&staging)?;
        self.validate_publication_charge_record(&binding)?;
        Ok(Some(binding))
    }
}

/// Borrowed original issuer inputs. Stored bytes cannot supply the private target or charge.
pub(crate) struct PublicationBindingCapture<'a> {
    pub(crate) canonical_command: &'a [u8],
    pub(crate) validation: &'a crate::file_operation_owner::FileOperationResult,
    pub(crate) charge: &'a crate::operation_journal::capacity_provider::PublicationChargeBinding,
    pub(crate) chunk: Option<ChunkId>,
    pub(crate) issuer: (Uuid, u64),
}
