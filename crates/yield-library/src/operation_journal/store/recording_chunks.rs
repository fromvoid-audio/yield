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

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PayloadReader {
    Control,
    Chunk,
}
#[cfg(test)]
thread_local! {
    pub(super) static PAYLOAD_READ_HOOK: std::cell::RefCell<Option<(PayloadReader, Box<dyn FnOnce(&rusqlite::Connection)>)>> = const { std::cell::RefCell::new(None) };
    pub(super) static PAYLOAD_COPIED_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
#[cfg(test)]
pub(super) fn payload_read_boundary(kind: PayloadReader, connection: &rusqlite::Connection) {
    let hook = PAYLOAD_READ_HOOK.with(|h| {
        let mut h = h.borrow_mut();
        if h.as_ref().is_some_and(|(reader, _)| *reader == kind) {
            h.take()
        } else {
            None
        }
    });
    if let Some((_, hook)) = hook {
        hook(connection);
    }
}

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
// Match the previous Vec<u8> conversion error without projecting/copying a wrong-type value.
pub(super) fn recording_payload_type_error(kind: &str) -> OperationJournalStoreError {
    use rusqlite::types::Type;
    let value_type = match kind {
        "integer" => Type::Integer,
        "real" => Type::Real,
        "text" => Type::Text,
        _ => Type::Null,
    };
    rusqlite::Error::InvalidColumnType(0, "record".into(), value_type).into()
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
        self.configure_recording_metadata_policy_with_input(None)
    }

    pub(super) fn configure_recording_metadata_policy_with_input(
        &self,
        input: Option<&crate::operation_journal::owner::InputPermit>,
    ) -> Result<(), OperationJournalStoreError> {
        use crate::operation_journal::recording_commands::MAX_RECORDING_JOURNAL_DATABASE_BYTES;
        self.validate_namespace_with_input(input, false)?;
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
        let existing: i64 = self
            .connection
            .query_row("PRAGMA max_page_count", [], |r| r.get(0))?;
        if existing <= 0 {
            return Err(error("unknown existing SQLite growth limit"));
        }
        // Recording may tighten the original owner's policy, never enlarge it.
        let pages = (MAX_RECORDING_JOURNAL_DATABASE_BYTES / page_size as u64).min(existing as u64);
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
    #[allow(dead_code)] // Preserve the original unfunded entry point.
    pub(crate) fn validate_recording_capacity_binding(
        &self,
        proposal: &RecordingProposal,
        plan: &crate::operation_journal::full_capacity::FullCapacityPlan,
    ) -> Result<(), OperationJournalStoreError> {
        self.validate_recording_capacity_binding_with_input(proposal, plan, None)
    }

    pub(in crate::operation_journal) fn validate_recording_capacity_binding_with_input(
        &self,
        proposal: &RecordingProposal,
        plan: &crate::operation_journal::full_capacity::FullCapacityPlan,
        input: Option<&crate::operation_journal::owner::InputPermit>,
    ) -> Result<(), OperationJournalStoreError> {
        self.configure_recording_metadata_policy_with_input(input)?;
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
            || crate::operation_journal::recording_commands::controls::reservation_metadata_bound(
                proposal,
            )
            .map_err(error)?
                > crate::operation_journal::recording_commands::MAX_RECORDING_RETAINED_ENCODED_BYTES
        {
            return Err(error(
                "recording journal peak is not covered on its actual descriptor volume",
            ));
        }
        self.validate_namespace_with_input(input, false)?;
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
        #[cfg(test)]
        payload_read_boundary(PayloadReader::Chunk, &self.connection);
        // The payload projection carries its own bound: the earlier scalar observation is
        // not relied on to bound this copy. Empty BLOBs still reach the original decoder.
        let (bytes, payload_kind): (Option<Vec<u8>>, String) = self.connection.query_row(
            "SELECT CASE WHEN typeof(record)='blob' AND length(record) BETWEEN 0 AND 131072 THEN record ELSE NULL END AS record, typeof(record) FROM operation_journal_recording_chunks WHERE chunk_id=?1",
            params![chunk_id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if payload_kind != "blob" {
            return Err(recording_payload_type_error(&payload_kind));
        }
        let bytes = bytes
            .ok_or_else(|| error("invalid recording observation: encoded value exceeds bound"))?;
        #[cfg(test)]
        PAYLOAD_COPIED_BYTES.with(|n| n.set(n.get() + bytes.len()));
        let chunk = RecordingCapturedChunk::decode(&bytes, proposal).map_err(error)?;
        if chunk.chunk_id() != chunk_id || slot(chunk.role()) != stored_slot {
            return Err(error("recording row slot/key differs from immutable bytes"));
        }
        Ok(Some((sequence, chunk)))
    }
    /// Audit the actual retained relation in the existing consistent rebuild transaction.
    /// Decode one bounded chunk at a time; observations never authorize capture or release.
    pub(super) fn validate_recording_inventory_inside_transaction(
        &self,
    ) -> Result<(), OperationJournalStoreError> {
        self.recording_retained_totals()?;
        let mut after_operation = String::new();
        loop {
            let mut statement = self.connection.prepare(
                "SELECT DISTINCT operation_id FROM operation_journal_recording_chunks
                 WHERE operation_id>?1 ORDER BY operation_id LIMIT 8",
            )?;
            let operations = statement
                .query_map([&after_operation], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            if operations.is_empty() {
                break;
            }
            for text in operations {
                let id = Uuid::parse_str(&text).map_err(error)?;
                if id.is_nil() || id.to_string() != text {
                    return Err(error(
                        "restart recording operation identity is not canonical",
                    ));
                }
                let proposal = self.recording_proposal(OperationId::from_uuid(id))?;
                self.verify_recording_reservation(&proposal, false)?;
                self.validate_recording_operation_inventory(&proposal)?;
                after_operation = text;
            }
        }
        Ok(())
    }

    fn validate_recording_operation_inventory(
        &self,
        proposal: &RecordingProposal,
    ) -> Result<(), OperationJournalStoreError> {
        let mut after_slot = -1i64;
        let mut next_pcm = 0u32;
        let mut next_header = 0u32;
        let mut previous_pcm = None;
        let mut previous_header = None;
        let mut first_pcm_sequence = None;
        let mut final_header_sequence = None;
        let mut initial_header_sequence = None;
        let mut previous_pcm_full = true;
        loop {
            // The private (operation_id,slot) index bounds both ordering and each key page.
            let mut statement = self.connection.prepare(
                "SELECT chunk_id,slot,sequence FROM operation_journal_recording_chunks
                 WHERE operation_id=?1 AND slot>?2 ORDER BY slot LIMIT 8",
            )?;
            let keys = statement
                .query_map(
                    params![proposal.operation_id().to_string(), after_slot],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, i64>(2)?,
                        ))
                    },
                )?
                .collect::<Result<Vec<_>, _>>()?;
            if keys.is_empty() {
                break;
            }
            for (text, stored_slot, sequence) in keys {
                let id = Uuid::parse_str(&text).map_err(error)?;
                if id.is_nil() || id.to_string() != text {
                    return Err(error("restart recording chunk identity is not canonical"));
                }
                self.ensure_sequence_unique(sequence)?;
                after_slot = stored_slot;
                if (20_000..=20_003).contains(&stored_slot) {
                    continue;
                }
                let (read_sequence, chunk) = self
                    .read_recording_chunk_inside(id, proposal)?
                    .ok_or_else(|| error("restart recording chunk disappeared"))?;
                if read_sequence != sequence {
                    return Err(error("restart recording chunk sequence changed"));
                }
                match chunk.role() {
                    RecordingChunkRole::Pcm { ordinal } => {
                        if ordinal != next_pcm
                            || !previous_pcm_full
                            || previous_pcm.is_some_and(|n| sequence <= n)
                        {
                            return Err(error("restart recording PCM prefix has an ordering gap"));
                        }
                        first_pcm_sequence.get_or_insert(sequence);
                        previous_pcm = Some(sequence);
                        previous_pcm_full =
                            chunk.bytes().len() as u64 == proposal.format().pcm_chunk_bytes();
                        next_pcm += 1;
                    }
                    RecordingChunkRole::Header { revision } => {
                        if revision != next_header || previous_header.is_some_and(|n| sequence <= n)
                        {
                            return Err(error(
                                "restart recording header prefix has an ordering gap",
                            ));
                        }
                        if revision == 0 {
                            initial_header_sequence = Some(sequence);
                        } else {
                            final_header_sequence.get_or_insert(sequence);
                        }
                        previous_header = Some(sequence);
                        next_header += 1;
                    }
                }
            }
        }
        if first_pcm_sequence
            .is_some_and(|pcm| initial_header_sequence.is_none_or(|header| pcm <= header))
            || previous_pcm
                .is_some_and(|pcm| final_header_sequence.is_some_and(|header| pcm >= header))
        {
            return Err(error(
                "restart recording capture crosses its header boundary",
            ));
        }
        Ok(())
    }

    /// Original immutable pre-start observation. This read issues no producer or write authority.
    pub(crate) fn recording_prestart_proposal(
        &self,
        operation: OperationId,
    ) -> Result<RecordingProposal, OperationJournalStoreError> {
        if !self.recording_prestart_scope.get() {
            self.begin_recording_prestart_read()?;
            let result = self.recording_prestart_proposal_inside(operation);
            self.finish_recording_prestart_read()?;
            return result;
        }
        self.recording_prestart_proposal_inside(operation)
    }

    fn recording_prestart_proposal_inside(
        &self,
        operation: OperationId,
    ) -> Result<RecordingProposal, OperationJournalStoreError> {
        self.validate_recording_prestart_namespace()?;
        let record = self
            .read_row(operation)?
            .ok_or_else(|| error("recording operation absent"))?
            .record;
        if record.schema_version != SCHEMA_V17
            || record.phase != super::super::model::OperationPhase::IntentDurable
            || record.disposition != OperationDisposition::None
        {
            return Err(error(
                "recording operation is not an unchanged pre-start intent",
            ));
        }
        let Some(UniversalCommandArgs::Recording(proposal)) = record.universal_command_args()
        else {
            return Err(error("operation has no recording proposal"));
        };
        self.verify_recording_reservation(proposal, true)?;
        let captured: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM operation_journal_recording_chunks WHERE operation_id=?1
             AND slot NOT BETWEEN 20000 AND 20003)",
            [operation.to_string()],
            |r| r.get(0),
        )?;
        if captured {
            return Err(error("recording already has captured bytes"));
        }
        let proposal = proposal.clone();
        self.validate_recording_prestart_namespace()?;
        Ok(proposal)
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
            self.verify_recording_reservation(&proposal, true)?;
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
            let (rows, bytes) = self.recording_retained_totals()?;
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
            let patched: bool = self.connection.query_row("SELECT EXISTS(SELECT 1 FROM operation_journal_recording_chunks WHERE operation_id=?1 AND slot BETWEEN 10001 AND 10015)", params![chunk.operation_id().to_string()], |r| r.get(0))?;
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
        self.verify_recording_reservation(&proposal, false)?;
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
        self.verify_recording_reservation(&proposal, false)?;
        let mut statement = self.connection.prepare("SELECT CASE WHEN typeof(chunk_id)='text' AND length(chunk_id)=36 THEN chunk_id ELSE NULL END,sequence FROM operation_journal_recording_chunks WHERE operation_id=?1 AND sequence>?2 AND slot NOT BETWEEN 20000 AND 20003 ORDER BY sequence LIMIT 9")?;
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
