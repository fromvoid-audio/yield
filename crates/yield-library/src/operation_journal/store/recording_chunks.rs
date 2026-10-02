//! Actual immutable captured bytes retained by the same journal connection/owner.
//!
//! Successful storage is an observation only: it cannot recreate a live source pin, grant,
//! output receipt, completed capture or retirement/reclamation authority.
use super::{OperationJournalStore, OperationJournalStoreError};
use crate::operation_journal::coordinator::UniversalCommandArgs;
use crate::operation_journal::model::{OperationDisposition, OperationId, SCHEMA_V17};
use crate::operation_journal::recording_commands::{
    RecordingCapturedChunk, RecordingChunkRole, RecordingProposal,
};
use rusqlite::{OptionalExtension, params};
use uuid::Uuid;

pub(super) const TABLE_NAME: &str = "operation_journal_recording_chunks";
pub(super) const CREATE_TABLE: &str = "CREATE TABLE operation_journal_recording_chunks (
    chunk_id TEXT NOT NULL PRIMARY KEY,
    operation_id TEXT NOT NULL,
    slot INTEGER NOT NULL,
    sequence INTEGER NOT NULL UNIQUE,
    record BLOB NOT NULL CHECK(length(record) <= 131072),
    UNIQUE(operation_id, slot)
)";
const MAX_ROWS: i64 = 8192;
const MAX_RETAINED_BYTES: i64 =
    crate::operation_journal::recording_commands::MAX_RECORDING_RETAINED_ENCODED_BYTES as i64;
const MAX_PAGE_ROWS: usize = 8;

/// Read-only storage fact, never a physical or live-dispatch receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RecordingChunkReceipt {
    Committed { sequence: i64 },
    Replay { sequence: i64 },
}
/// A bounded page of actual recovered bytes; incomplete capture stays unpublished.
#[derive(Clone, Debug)]
#[allow(dead_code)] // Consumed by the recording recovery adapter in OPT-2058.
pub(crate) struct RecordingChunkPage {
    pub(crate) chunks: Vec<RecordingCapturedChunk>,
    pub(crate) next_sequence: Option<i64>,
}
fn error(detail: impl ToString) -> OperationJournalStoreError {
    OperationJournalStoreError::RecordingRetention {
        reason: detail.to_string(),
    }
}
fn slot(role: RecordingChunkRole) -> i64 {
    match role {
        RecordingChunkRole::Pcm { ordinal } => i64::from(ordinal),
        RecordingChunkRole::Header { revision } => 10_000 + i64::from(revision),
    }
}

impl OperationJournalStore {
    /// Enforce the assumptions behind the real fixed metadata peak; unknown SQLite builds or
    /// changed schema/pager policy are rejected before any captured-byte transaction.
    pub(super) fn configure_recording_metadata_policy(
        &self,
    ) -> Result<(), OperationJournalStoreError> {
        use crate::operation_journal::recording_commands::MAX_RECORDING_JOURNAL_DATABASE_BYTES;
        self.validate_namespace()?;
        if rusqlite::version_number() != 3_050_002 {
            return Err(error(
                "recording metadata bound requires bundled SQLite 3.50.2",
            ));
        }
        super::validate_existing_schema(
            &self.connection,
            &self.database_path,
            super::JOURNAL_SCHEMA_VERSION,
        )?;
        let temporary: i64 =
            self.connection
                .query_row("SELECT COUNT(*) FROM sqlite_temp_master", [], |r| r.get(0))?;
        if temporary != 0 {
            return Err(error(
                "recording metadata policy forbids temporary schema objects",
            ));
        }
        self.connection
            .execute_batch("PRAGMA cache_spill=OFF; PRAGMA temp_store=MEMORY;")?;
        let page_size: i64 = self
            .connection
            .query_row("PRAGMA page_size", [], |r| r.get(0))?;
        if !(512..=65536).contains(&page_size) || !(page_size as u64).is_power_of_two() {
            return Err(error("unknown SQLite page size"));
        }
        let pages = MAX_RECORDING_JOURNAL_DATABASE_BYTES / page_size as u64;
        let enforced: i64 =
            self.connection
                .query_row(&format!("PRAGMA max_page_count={pages}"), [], |r| r.get(0))?;
        let current: i64 = self
            .connection
            .query_row("PRAGMA page_count", [], |r| r.get(0))?;
        let spill: i64 = self
            .connection
            .query_row("PRAGMA cache_spill", [], |r| r.get(0))?;
        let temp: i64 = self
            .connection
            .query_row("PRAGMA temp_store", [], |r| r.get(0))?;
        if enforced != pages as i64 || current < 0 || current > enforced || spill != 0 || temp != 2
        {
            return Err(error(
                "recording SQLite growth policy unavailable or exceeded",
            ));
        }
        Ok(())
    }
    pub(crate) fn validate_recording_capacity_binding(
        &self,
        proposal: &RecordingProposal,
        plan: &crate::operation_journal::full_capacity::FullCapacityPlan,
    ) -> Result<(), OperationJournalStoreError> {
        self.configure_recording_metadata_policy()?;
        let opened = crate::sample_sources::db::sqlite_identity::open_existing_database_file(
            &self.database_root,
            std::path::Path::new(super::JOURNAL_DATABASE_FILE_NAME),
            &self.database_path,
        )
        .map_err(super::map_identity_error)?;
        if opened.identity != self.database_identity {
            return Err(error("recording journal descriptor replaced"));
        }
        let facts =
            crate::durable_capacity::descriptor_capacity_facts(&opened.file).map_err(error)?;
        let journal_bytes = plan.allocations().iter().filter(|a| a.class() == crate::operation_journal::full_capacity::FullCapacityAllocationClass::JournalRecord && a.volume() == &facts.identity).try_fold(0u64, |sum,a|sum.checked_add(a.logical_bytes())).ok_or_else(||error("journal capacity bound overflow"))?;
        if journal_bytes
            < crate::operation_journal::recording_commands::RECORDING_JOURNAL_PEAK_BYTES
            || proposal.retained_metadata_bound().map_err(error)?
                > crate::operation_journal::recording_commands::MAX_RECORDING_RETAINED_ENCODED_BYTES
        {
            return Err(error(
                "recording journal peak is not covered on its actual descriptor volume",
            ));
        }
        self.validate_namespace()?;
        Ok(())
    }
    fn recording_proposal(
        &self,
        operation_id: OperationId,
    ) -> Result<RecordingProposal, OperationJournalStoreError> {
        let operation = self
            .read_row(operation_id)?
            .ok_or_else(|| error("recording operation absent"))?
            .record;
        let Some(UniversalCommandArgs::Recording(proposal)) = operation.universal_command_args()
        else {
            return Err(error("operation has no recording proposal"));
        };
        if operation.schema_version != SCHEMA_V17 {
            return Err(error("recording schema mismatch"));
        }
        Ok(proposal.clone())
    }
    fn read_recording_chunk_inside(
        &self,
        chunk_id: Uuid,
        proposal: &RecordingProposal,
    ) -> Result<Option<(i64, RecordingCapturedChunk)>, OperationJournalStoreError> {
        // SQL type/length checks run before asking rusqlite to materialize an untrusted blob.
        let row: Option<(String, i64, i64, String, i64, i64)> = self.connection.query_row(
            "SELECT CASE WHEN typeof(operation_id)='text' AND length(operation_id)=36 THEN operation_id ELSE NULL END, slot, sequence, typeof(record), length(record), length(chunk_id) FROM operation_journal_recording_chunks WHERE chunk_id=?1",
            params![chunk_id.to_string()], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
        let Some((operation, stored_slot, sequence, kind, length, key_length)) = row else {
            return Ok(None);
        };
        if operation != proposal.operation_id().to_string()
            || sequence <= 0
            || kind != "blob"
            || !(1..=131072).contains(&length)
            || key_length != 36
        {
            return Err(error("retained recording row shape/binding"));
        }
        let bytes: Vec<u8> = self.connection.query_row(
            "SELECT record FROM operation_journal_recording_chunks WHERE chunk_id=?1",
            params![chunk_id.to_string()],
            |r| r.get(0),
        )?;
        let chunk = RecordingCapturedChunk::decode(&bytes, proposal).map_err(error)?;
        if chunk.chunk_id() != chunk_id || slot(chunk.role()) != stored_slot {
            return Err(error("recording row slot/key differs from immutable bytes"));
        }
        Ok(Some((sequence, chunk)))
    }
    pub(crate) fn retain_recording_chunk(
        &self,
        chunk: &RecordingCapturedChunk,
    ) -> Result<RecordingChunkReceipt, OperationJournalStoreError> {
        self.validate_namespace()?;
        self.configure_recording_metadata_policy()?;
        self.connection.execute_batch("BEGIN EXCLUSIVE")?;
        let result = (|| {
            self.validate_namespace_without_sidecars()?;
            let proposal = self.recording_proposal(chunk.operation_id())?;
            let encoded = chunk.encode(&proposal).map_err(error)?;
            if let Some((sequence, existing)) =
                self.read_recording_chunk_inside(chunk.chunk_id(), &proposal)?
            {
                existing.validate_replay(chunk, &proposal).map_err(error)?;
                return Ok(RecordingChunkReceipt::Replay { sequence });
            }
            let operation = self
                .read_row(chunk.operation_id())?
                .ok_or_else(|| error("operation disappeared"))?
                .record;
            if operation.disposition != OperationDisposition::None {
                return Err(error(
                    "capture is cancelled/retry/audit; new bytes not admitted",
                ));
            }
            let occupied: bool = self.connection.query_row("SELECT EXISTS(SELECT 1 FROM operation_journal_recording_chunks WHERE operation_id=?1 AND slot=?2)", params![chunk.operation_id().to_string(), slot(chunk.role())], |r| r.get(0))?;
            if occupied {
                return Err(error(
                    "recording slot has a different immutable chunk identity",
                ));
            }
            let (rows, bytes): (i64,i64) = self.connection.query_row("SELECT COUNT(*), COALESCE(SUM(bytes),0) FROM (SELECT length(record) AS bytes FROM operation_journal_recording_chunks LIMIT 8193)", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
            if rows >= MAX_ROWS
                || bytes
                    .checked_add(encoded.len() as i64)
                    .is_none_or(|n| n > MAX_RETAINED_BYTES)
            {
                return Err(error(
                    "recording retention exhausted; prefix remains retained",
                ));
            }
            self.validate_recording_append(chunk, &proposal)?;
            let sequence = self.next_sequence_inside_transaction()?;
            self.connection.execute("INSERT INTO operation_journal_recording_chunks(chunk_id,operation_id,slot,sequence,record) VALUES(?1,?2,?3,?4,?5)", params![chunk.chunk_id().to_string(), chunk.operation_id().to_string(), slot(chunk.role()), sequence, encoded])?;
            #[cfg(test)]
            if let Some(
                super::InsertTestDirective::Fail(e)
                | super::InsertTestDirective::FailAndLeaveForRollback(e)
                | super::InsertTestDirective::ExecuteThenFail(e),
            ) = self.insert_test_directive(super::InsertTestBoundary::BeforePrecommitValidation)
            {
                return Err(e);
            }
            self.validate_namespace_without_sidecars()?;
            Ok(RecordingChunkReceipt::Committed { sequence })
        })();
        match result {
            Ok(receipt) => {
                #[cfg(test)]
                match self.insert_test_directive(super::InsertTestBoundary::Commit) {
                    Some(
                        super::InsertTestDirective::Fail(e)
                        | super::InsertTestDirective::FailAndLeaveForRollback(e),
                    ) => {
                        let _ = self.connection.execute_batch("ROLLBACK");
                        return Err(e);
                    }
                    Some(super::InsertTestDirective::ExecuteThenFail(e)) => {
                        self.connection.execute_batch("COMMIT")?;
                        return Err(e);
                    }
                    _ => {}
                }
                if let Err(e) = self.connection.execute_batch("COMMIT") {
                    let _ = self.connection.execute_batch("ROLLBACK");
                    return Err(e.into());
                }
                #[cfg(test)]
                if let Some(
                    super::InsertTestDirective::Fail(e)
                    | super::InsertTestDirective::FailAndLeaveForRollback(e)
                    | super::InsertTestDirective::ExecuteThenFail(e),
                ) = self.insert_test_directive(super::InsertTestBoundary::AfterCommit)
                {
                    return Err(e);
                }
                self.validate_namespace()?;
                let proposal = self.recording_proposal(chunk.operation_id())?;
                let stored = self
                    .read_recording_chunk_inside(chunk.chunk_id(), &proposal)?
                    .ok_or_else(|| error("committed recording bytes unavailable for readback"))?
                    .1;
                stored.validate_replay(chunk, &proposal).map_err(error)?;
                self.validate_namespace()?;
                Ok(receipt)
            }
            Err(e) => {
                let _ = self.connection.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }
    fn validate_recording_append(
        &self,
        chunk: &RecordingCapturedChunk,
        proposal: &RecordingProposal,
    ) -> Result<(), OperationJournalStoreError> {
        let operation = chunk.operation_id().to_string();
        let previous_slot = match chunk.role() {
            RecordingChunkRole::Header { revision: 0 } => return Ok(()),
            RecordingChunkRole::Header { revision } => 10_000 + i64::from(revision - 1),
            RecordingChunkRole::Pcm { ordinal: 0 } => 10_000,
            RecordingChunkRole::Pcm { ordinal } => i64::from(ordinal - 1),
        };
        let previous: Option<String> = self.connection.query_row("SELECT CASE WHEN typeof(chunk_id)='text' AND length(chunk_id)=36 THEN chunk_id ELSE NULL END FROM operation_journal_recording_chunks WHERE operation_id=?1 AND slot=?2", params![operation, previous_slot], |r| r.get(0)).optional()?;
        let previous_id = previous.ok_or_else(|| error("recording prefix/header ordering gap"))?;
        let previous = self
            .read_recording_chunk_inside(Uuid::parse_str(&previous_id).map_err(error)?, proposal)?
            .ok_or_else(|| error("recording previous slot absent"))?
            .1;
        if matches!(chunk.role(), RecordingChunkRole::Pcm { ordinal } if ordinal > 0)
            && previous.bytes().len() as u64 != proposal.format().pcm_chunk_bytes()
        {
            return Err(error("cannot append after a short final PCM slot"));
        }
        if matches!(chunk.role(), RecordingChunkRole::Pcm { .. }) {
            let patched: bool = self.connection.query_row("SELECT EXISTS(SELECT 1 FROM operation_journal_recording_chunks WHERE operation_id=?1 AND slot>10000)", params![chunk.operation_id().to_string()], |r| r.get(0))?;
            if patched {
                return Err(error("capture prefix already entered header finalization"));
            }
        }
        Ok(())
    }
    pub(crate) fn get_recording_chunk(
        &self,
        operation_id: OperationId,
        chunk_id: Uuid,
    ) -> Result<Option<RecordingCapturedChunk>, OperationJournalStoreError> {
        self.validate_namespace()?;
        let proposal = self.recording_proposal(operation_id)?;
        let result = self
            .read_recording_chunk_inside(chunk_id, &proposal)?
            .map(|(_, c)| c);
        self.validate_namespace()?;
        Ok(result)
    }
    pub(crate) fn recording_chunk_page(
        &self,
        operation_id: OperationId,
        after_sequence: Option<i64>,
    ) -> Result<RecordingChunkPage, OperationJournalStoreError> {
        self.validate_namespace()?;
        if after_sequence.is_some_and(|n| n <= 0) {
            return Err(error("invalid recording recovery cursor"));
        }
        let proposal = self.recording_proposal(operation_id)?;
        let mut statement = self.connection.prepare("SELECT CASE WHEN typeof(chunk_id)='text' AND length(chunk_id)=36 THEN chunk_id ELSE NULL END,sequence FROM operation_journal_recording_chunks WHERE operation_id=?1 AND sequence>?2 ORDER BY sequence LIMIT 9")?;
        let keys = statement
            .query_map(
                params![operation_id.to_string(), after_sequence.unwrap_or(0)],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = keys.len() > MAX_PAGE_ROWS;
        let mut chunks = Vec::new();
        let mut last = None;
        for (id, sequence) in keys.into_iter().take(MAX_PAGE_ROWS) {
            let chunk = self
                .read_recording_chunk_inside(Uuid::parse_str(&id).map_err(error)?, &proposal)?
                .ok_or_else(|| error("recovery chunk disappeared"))?;
            if chunk.0 != sequence {
                return Err(error("recovery sequence changed"));
            }
            chunks.push(chunk.1);
            last = Some(sequence);
        }
        self.validate_namespace()?;
        Ok(RecordingChunkPage {
            chunks,
            next_sequence: if has_more { last } else { None },
        })
    }
}
