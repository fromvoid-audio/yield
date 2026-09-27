//! Versioned, authority-free incomplete chunk staging checkpoints.
//!
//! An Issued claim is not progress. Only a live owner may persist a positive checkpoint after
//! verifying the exact staged object. Stored rows cannot reissue a chunk or release its charge.

use rusqlite::types::ValueRef;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::file_operation_owner::FileOperationResult;
use crate::operation_journal::codec::parse_unique_value;
use crate::operation_journal::model::{OperationDisposition, OperationId, SCHEMA_V16};
use crate::operation_journal::participants::ParticipantId;

use super::chunk_claims::{ChunkClaimRecord, ChunkClaimState, ChunkId};
use super::staging_checkpoints::StagingCheckpointRecord;
#[cfg(test)]
use super::{InsertTestBoundary, InsertTestDirective};
use super::{OperationJournalStore, OperationJournalStoreError};

pub(super) const TABLE_NAME: &str = "operation_journal_chunk_staging";
pub(super) const CREATE_TABLE: &str = "CREATE TABLE operation_journal_chunk_staging (
    chunk_id TEXT NOT NULL PRIMARY KEY,
    sequence INTEGER NOT NULL UNIQUE,
    record BLOB NOT NULL CHECK(length(record) <= 65536)
)";

pub(super) const MAX_RECORD_BYTES: usize = 64 * 1024;
pub(super) const MAX_ROWS: i64 = 8192;

const RECORD_SCHEMA_VERSION: u32 = 1;

/// A versioned durable status observation; V1-V16 operation records remain unchanged.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum PartialChunkStatus {
    PartialNeedsRetry,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ChunkStagingRecord {
    schema_version: u32,
    chunk_id: Uuid,
    operation_id: OperationId,
    issue_id: Uuid,
    participant_id: ParticipantId,
    output_key: String,
    logical_offset: u64,
    logical_length: u64,
    status: PartialChunkStatus,
    staging: StagingCheckpointRecord,
}

impl ChunkStagingRecord {
    pub(super) fn from_live(
        claim: &ChunkClaimRecord,
        issue_id: Uuid,
        validation: &FileOperationResult,
    ) -> Result<Self, String> {
        let staging = StagingCheckpointRecord::from_file_result(validation)?;
        let record = Self {
            schema_version: RECORD_SCHEMA_VERSION,
            chunk_id: claim.chunk_id().as_uuid(),
            operation_id: claim.operation_id(),
            issue_id,
            participant_id: claim.participant_id(),
            output_key: claim.output_key().to_owned(),
            logical_offset: claim.logical_offset(),
            logical_length: claim.logical_length(),
            status: PartialChunkStatus::PartialNeedsRetry,
            staging,
        };
        if claim.state() != ChunkClaimState::Issued || claim.issue_id() != Some(issue_id) {
            return Err(String::from("chunk has no exact Issued identity"));
        }
        record.validate()?;
        Ok(record)
    }

    pub(super) fn chunk_id(&self) -> Result<ChunkId, String> {
        ChunkId::from_uuid(self.chunk_id).map_err(|error| error.to_string())
    }

    pub(super) fn staging(&self) -> &StagingCheckpointRecord {
        &self.staging
    }

    pub(super) fn validate(&self) -> Result<(), String> {
        if self.schema_version != RECORD_SCHEMA_VERSION
            || self.chunk_id.is_nil()
            || self.operation_id.as_uuid().is_nil()
            || self.issue_id.is_nil()
            || self.participant_id.as_uuid().is_nil()
            || self.output_key.is_empty()
            || self.output_key.len() > 1024
            || self.logical_length == 0
            || self
                .logical_offset
                .checked_add(self.logical_length)
                .is_none()
            || self.staging.bytes() == 0
            || self.staging.operation_id() != self.operation_id
            || self.staging.participant_id() != self.participant_id
        {
            return Err(String::from(
                "chunk staging checkpoint shape or binding is invalid",
            ));
        }
        self.staging.validate()
    }

    pub(super) fn matches_claim(&self, claim: &ChunkClaimRecord) -> bool {
        self.chunk_id == claim.chunk_id().as_uuid()
            && self.operation_id == claim.operation_id()
            && self.issue_id == claim.issue_id().unwrap_or(Uuid::nil())
            && self.participant_id == claim.participant_id()
            && self.output_key == claim.output_key()
            && self.logical_offset == claim.logical_offset()
            && self.logical_length == claim.logical_length()
            && claim.state() == ChunkClaimState::Issued
    }

    pub(super) fn encode(&self) -> Result<Vec<u8>, String> {
        self.validate()?;
        let encoded = serde_json::to_vec(self).map_err(|error| error.to_string())?;
        if encoded.len() > MAX_RECORD_BYTES {
            return Err(String::from(
                "chunk staging checkpoint exceeds record bound",
            ));
        }
        Ok(encoded)
    }

    pub(super) fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(String::from(
                "chunk staging checkpoint exceeds record bound",
            ));
        }
        let value = parse_unique_value(bytes).map_err(|error| error.to_string())?;
        let record: Self = serde_json::from_value(value).map_err(|error| error.to_string())?;
        record.validate()?;
        Ok(record)
    }
}

/// A read-only checkpoint receipt. Neither variant can reissue or release a chunk.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ChunkStagingReceipt {
    Committed { sequence: i64 },
    Replay { sequence: i64 },
}

fn checkpoint_error(reason: impl Into<String>) -> OperationJournalStoreError {
    OperationJournalStoreError::ChunkStaging {
        reason: reason.into(),
    }
}

impl OperationJournalStore {
    /// Inspect bounded durable checkpoint rows as observations only. An Issued claim by itself
    /// never proves partial work, and a malformed retained row cannot silently become absence.
    pub(super) fn has_valid_chunk_staging(
        &self,
        operation_id: OperationId,
    ) -> Result<bool, OperationJournalStoreError> {
        self.validate_namespace()?;
        let mut statement = self.connection.prepare(
            "SELECT chunk_id FROM operation_journal_chunk_staging ORDER BY sequence ASC LIMIT ?1",
        )?;
        let ids = statement
            .query_map(params![MAX_ROWS + 1], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        if ids.len() as i64 > MAX_ROWS {
            return Err(checkpoint_error("chunk checkpoint row bound is exceeded"));
        }
        let mut present = false;
        for key in ids {
            let parsed = Uuid::parse_str(&key)
                .map_err(|_| checkpoint_error("chunk checkpoint key is not a UUID"))?;
            let chunk_id =
                ChunkId::from_uuid(parsed).map_err(|error| checkpoint_error(error.to_string()))?;
            if chunk_id.to_string() != key {
                return Err(checkpoint_error("chunk checkpoint key is not canonical"));
            }
            let (_, record) = self.read_chunk_staging_row(chunk_id)?.ok_or_else(|| {
                checkpoint_error("chunk checkpoint disappeared during bounded scan")
            })?;
            let claim = super::chunk_claims::read(&self.connection, chunk_id)
                .map_err(|error| checkpoint_error(error.to_string()))?
                .ok_or_else(|| checkpoint_error("chunk checkpoint has no retained claim"))?;
            if !record.matches_claim(&claim) {
                return Err(checkpoint_error(
                    "chunk checkpoint differs from retained claim",
                ));
            }
            if record.operation_id == operation_id {
                let operation = self
                    .read_row(operation_id)?
                    .ok_or_else(|| checkpoint_error("checkpoint operation is absent"))?
                    .record;
                let participants = operation
                    .universal_intent
                    .as_ref()
                    .ok_or_else(|| checkpoint_error("checkpoint participant set is absent"))?
                    .participants()
                    .descriptors();
                let participant_ok = participants
                    .iter()
                    .any(|participant| participant.id() == record.participant_id);
                let source_ok = record.staging().source_fence().is_some_and(|source| {
                    participants
                        .iter()
                        .filter_map(|participant| participant.source_fence())
                        .any(|retained| source.matches_participant(retained))
                });
                if operation.schema_version != SCHEMA_V16 || !participant_ok || !source_ok {
                    return Err(checkpoint_error(
                        "chunk checkpoint differs from durable operation participants",
                    ));
                }
                present = true;
            }
        }
        Ok(present)
    }

    pub(crate) fn checkpoint_chunk_staging(
        &self,
        chunk_id: ChunkId,
        issue_id: Uuid,
        validation: &FileOperationResult,
    ) -> Result<ChunkStagingReceipt, OperationJournalStoreError> {
        if issue_id.is_nil() {
            return Err(checkpoint_error("chunk issue identity is nil"));
        }
        self.validate_namespace()?;
        let claim = super::chunk_claims::read(&self.connection, chunk_id)
            .map_err(|error| checkpoint_error(error.to_string()))?
            .ok_or_else(|| checkpoint_error("chunk claim is absent"))?;
        let record = ChunkStagingRecord::from_live(&claim, issue_id, validation)
            .map_err(checkpoint_error)?;
        self.checkpoint_chunk_staging_record(&record)
    }

    /// The transactional core also accepts a typed fixture so fault tests can exercise storage
    /// independently of the file owner's separate live-capability validation tests.
    pub(super) fn checkpoint_chunk_staging_record(
        &self,
        record: &ChunkStagingRecord,
    ) -> Result<ChunkStagingReceipt, OperationJournalStoreError> {
        let chunk_id = record.chunk_id().map_err(checkpoint_error)?;
        let encoded = record.encode().map_err(checkpoint_error)?;
        self.validate_namespace()?;
        self.connection.execute_batch("BEGIN EXCLUSIVE")?;
        let outcome = (|| {
            let claim = super::chunk_claims::read(&self.connection, chunk_id)
                .map_err(|error| checkpoint_error(error.to_string()))?
                .ok_or_else(|| checkpoint_error("chunk claim is absent"))?;
            if !record.matches_claim(&claim) {
                return Err(checkpoint_error(
                    "chunk claim differs from submitted checkpoint",
                ));
            }
            let operation = self
                .read_row(record.operation_id)?
                .ok_or_else(|| checkpoint_error("chunk operation is absent"))?
                .record;
            if operation.schema_version != SCHEMA_V16
                || !operation.phase.is_pre_publication()
                || operation.disposition != OperationDisposition::None
            {
                return Err(checkpoint_error(
                    "chunk operation is cancelled, terminal, or past publication",
                ));
            }
            let participants = operation
                .universal_intent
                .as_ref()
                .ok_or_else(|| checkpoint_error("chunk operation has no participant set"))?
                .participants()
                .descriptors();
            if !participants
                .iter()
                .any(|participant| participant.id() == record.participant_id)
            {
                return Err(checkpoint_error(
                    "chunk participant is absent from durable intent",
                ));
            }
            let source_fence = record
                .staging()
                .source_fence()
                .ok_or_else(|| checkpoint_error("chunk staging proof has no source fence"))?;
            if !participants
                .iter()
                .filter_map(|participant| participant.source_fence())
                .any(|retained| source_fence.matches_participant(retained))
            {
                return Err(checkpoint_error(
                    "chunk source fence differs from durable participants",
                ));
            }
            if let Some((sequence, existing)) = self.read_chunk_staging_row(chunk_id)? {
                if existing != *record {
                    return Err(checkpoint_error(
                        "chunk has a conflicting staging checkpoint",
                    ));
                }
                return Ok(ChunkStagingReceipt::Replay { sequence });
            }
            let count: i64 = self.connection.query_row(
                "SELECT COUNT(*) FROM operation_journal_chunk_staging",
                [],
                |row| row.get(0),
            )?;
            if count >= MAX_ROWS {
                return Err(checkpoint_error(
                    "chunk staging checkpoint row bound is exhausted",
                ));
            }
            let sequence = self.next_sequence_inside_transaction()?;
            self.connection.execute(
                "INSERT INTO operation_journal_chunk_staging (chunk_id, sequence, record) VALUES (?1, ?2, ?3)",
                params![chunk_id.to_string(), sequence, encoded],
            )?;
            #[cfg(test)]
            if let Some(InsertTestDirective::Fail(error)) =
                self.insert_test_directive(InsertTestBoundary::BeforePrecommitValidation)
            {
                return Err(checkpoint_error(format!(
                    "chunk checkpoint precommit failed: {error}"
                )));
            }
            self.validate_namespace_for_precommit()?;
            Ok(ChunkStagingReceipt::Committed { sequence })
        })();
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                let _ = self.connection.execute_batch("ROLLBACK");
                return Err(error);
            }
        };
        if matches!(outcome, ChunkStagingReceipt::Replay { .. }) {
            self.connection.execute_batch("ROLLBACK")?;
            self.validate_namespace()?;
            return Ok(outcome);
        }
        #[cfg(test)]
        let commit_directive = self.insert_test_directive(InsertTestBoundary::Commit);
        #[cfg(test)]
        if let Some(InsertTestDirective::Fail(error)) = commit_directive {
            let _ = self.connection.execute_batch("ROLLBACK");
            return Err(checkpoint_error(format!(
                "chunk checkpoint commit was rejected: {error}"
            )));
        }
        #[cfg(test)]
        if let Some(InsertTestDirective::ExecuteThenFail(error)) = commit_directive {
            let commit = self.connection.execute_batch("COMMIT");
            let _ = self.connection.execute_batch("ROLLBACK");
            return Err(checkpoint_error(format!(
                "chunk checkpoint commit outcome is uncertain: {error}; {commit:?}"
            )));
        }
        self.connection.execute_batch("COMMIT")?;
        #[cfg(test)]
        if let Some(InsertTestDirective::Fail(error)) =
            self.insert_test_directive(InsertTestBoundary::AfterCommit)
        {
            return Err(checkpoint_error(format!(
                "chunk checkpoint committed without acknowledgement: {error}"
            )));
        }
        self.validate_namespace()?;
        let (sequence, readback) = self
            .read_chunk_staging_row(chunk_id)?
            .ok_or_else(|| checkpoint_error("chunk checkpoint disappeared during readback"))?;
        let claim = super::chunk_claims::read(&self.connection, chunk_id)
            .map_err(|error| checkpoint_error(error.to_string()))?
            .ok_or_else(|| {
                checkpoint_error("chunk claim disappeared during checkpoint readback")
            })?;
        if !matches!(outcome, ChunkStagingReceipt::Committed { sequence: expected } if expected == sequence)
            || readback != *record
            || !readback.matches_claim(&claim)
        {
            return Err(checkpoint_error(
                "chunk checkpoint readback differs from submitted proof",
            ));
        }
        Ok(outcome)
    }

    pub(super) fn read_chunk_staging_row(
        &self,
        chunk_id: ChunkId,
    ) -> Result<Option<(i64, ChunkStagingRecord)>, OperationJournalStoreError> {
        let raw = self.connection.query_row(
            "SELECT typeof(chunk_id), typeof(sequence), sequence, typeof(record), length(record), record FROM operation_journal_chunk_staging WHERE chunk_id = ?1",
            params![chunk_id.to_string()],
            |row| Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                match row.get_ref(2)? { ValueRef::Integer(value) => Some(value), _ => None },
                row.get::<_, String>(3)?,
                match row.get_ref(4)? { ValueRef::Integer(value) => Some(value), _ => None },
                match row.get_ref(5)? { ValueRef::Blob(value) => Some(value.to_vec()), _ => None },
            )),
        ).optional()?;
        let Some((key_type, sequence_type, sequence, record_type, length, blob)) = raw else {
            return Ok(None);
        };
        let sequence =
            sequence.ok_or_else(|| checkpoint_error("chunk sequence is not an integer"))?;
        let length =
            length.ok_or_else(|| checkpoint_error("chunk record length is not an integer"))?;
        let blob = blob.ok_or_else(|| checkpoint_error("chunk record is not a blob"))?;
        if key_type != "text"
            || sequence_type != "integer"
            || sequence <= 0
            || record_type != "blob"
            || length < 0
            || length as usize != blob.len()
            || blob.len() > MAX_RECORD_BYTES
        {
            return Err(checkpoint_error(
                "chunk checkpoint has malformed SQLite cells",
            ));
        }
        let record = ChunkStagingRecord::decode(&blob).map_err(checkpoint_error)?;
        if record.chunk_id().map_err(checkpoint_error)? != chunk_id {
            return Err(checkpoint_error("chunk checkpoint key differs from record"));
        }
        self.ensure_sequence_unique(sequence)?;
        Ok(Some((sequence, record)))
    }
}
