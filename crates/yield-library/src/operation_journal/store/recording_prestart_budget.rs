//! Complete scalar-only resource preflight for the narrow original pre-start factory.
//! These checks never validate a partial inventory, decode bytes or grant effects.
use super::*;

const MAX_ROWS: i64 = 32;
pub(super) const MAX_RECORD_BYTES: i64 = 64 * 1024;
pub(super) const JSON_LIMITS: super::super::codec::JsonMaterializationLimits =
    super::super::codec::JsonMaterializationLimits {
        maximum_nodes: 448,
        maximum_array_entries: 448,
        maximum_object_entries: 448,
        maximum_owned_string_bytes: 32 * 1024,
        maximum_depth: 32,
    };
const MAX_TOTAL_RECORD_BYTES: i64 = 128 * 1024;
const MAX_RECORDING_ROWS: i64 = 16;

// Each dynamic typed element in the supported Recording schema is one of these
// participant/checkpoint/allocation entries. Refuse compiler layout growth at build time.
const _: () = {
    assert!(std::mem::size_of::<super::super::participants::ParticipantDescriptor>() <= 1024);
    assert!(std::mem::size_of::<super::super::participants::ParticipantCheckpoint>() <= 512);
    assert!(std::mem::size_of::<super::super::full_capacity::FullCapacityAllocation>() <= 512);
    assert!(std::mem::size_of::<StoredOperationRecord>() <= 64 * 1024);
    assert!(std::mem::size_of::<serde_json::Value>() <= 80);
    assert!(std::mem::size_of::<String>() <= 24);
    assert!(std::mem::size_of::<usize>() <= 8);
};

// Peak categories overlap conservatively: original + validation-clone JSON trees,
// typed record + proposal validation clone, independent current proposal, raw/scratch,
// capped re-encoding, complete small claim/release maps and auxiliary SQL/key context.
// This is incremental factory backing; the original connection/cache/runtime already
// exist and are not created or enlarged by this preparation.
pub(crate) const COMMAND_INPUT_BYTES: usize = 3 * 1024 * 1024 + 512 * 1024 - 64 * 1024;
const TREE_BYTES: usize = 448 * 1536 + 32 * 1024;
const TYPED_BYTES: usize = 448 * 1024 + 32 * 1024 + 64 * 1024;
const LOCAL_PROPOSAL_BYTES: usize = 192 * 1024;
const RAW_AND_SCRATCH_BYTES: usize = 64 * 1024 + 128 * 1024;
const ENCODE_BYTES: usize = 3 * 128 * 1024;
const REBUILD_BYTES: usize = 128 * 1024;
const AUXILIARY_BYTES: usize = 64 * 1024;
const _: () = assert!(
    2 * TREE_BYTES
        + 2 * TYPED_BYTES
        + LOCAL_PROPOSAL_BYTES
        + RAW_AND_SCRATCH_BYTES
        + ENCODE_BYTES
        + REBUILD_BYTES
        + AUXILIARY_BYTES
        <= COMMAND_INPUT_BYTES
);

impl OperationJournalStore {
    pub(crate) fn begin_recording_prestart_read(&self) -> Result<(), OperationJournalStoreError> {
        self.validate_namespace()?;
        self.connection.execute_batch("BEGIN")?;
        if let Err(error) = self.preflight_recording_prestart_resources() {
            let _ = self.connection.execute_batch("ROLLBACK");
            return Err(error);
        }
        self.recording_prestart_resource_refused.set(false);
        self.recording_prestart_scope.set(true);
        if let Err(error) = self.prestart_decode_complete_record_inventory() {
            let _ = self.finish_recording_prestart_read();
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn finish_recording_prestart_read(&self) -> Result<(), OperationJournalStoreError> {
        self.recording_prestart_scope.set(false);
        self.recording_prestart_resource_refused.set(false);
        self.connection.execute_batch("ROLLBACK")?;
        self.validate_namespace()
    }

    pub(super) fn validate_recording_prestart_namespace(
        &self,
    ) -> Result<(), OperationJournalStoreError> {
        if self.recording_prestart_scope.get() && !self.connection.is_autocommit() {
            self.validate_namespace_for_precommit()
        } else {
            self.validate_namespace()
        }
    }

    pub(crate) fn rebuild_recording_prestart_snapshot(
        &self,
    ) -> Result<OrdinaryClaimRebuildSnapshot, OperationJournalStoreError> {
        if !self.recording_prestart_scope.get() || self.connection.is_autocommit() {
            return Err(OperationJournalStoreError::RecordingPrestartResourceBound);
        }
        let snapshot = self.rebuild_ordinary_claim_snapshot_inside_transaction()?;
        self.validate_chunk_claims_inside_transaction()?;
        if self.recording_prestart_resource_refused.get() {
            return Err(OperationJournalStoreError::RecordingPrestartResourceBound);
        }
        recovery_reserve::prove_no_active_charges(&self.connection).map_err(|error| {
            audit_error(
                &self.database_path,
                format!("recovery reserve admission audit failed: {error}"),
            )
        })?;
        self.validate_namespace_for_precommit()?;
        Ok(snapshot)
    }

    /// Call only inside the original consistent read transaction, before owned row reads.
    pub(super) fn preflight_recording_prestart_resources(
        &self,
    ) -> Result<(), OperationJournalStoreError> {
        if self.connection.is_autocommit() {
            return Err(OperationJournalStoreError::RecordingPrestartResourceBound);
        }
        // Counts are complete within the snapshot. A capped count refuses excess inventory;
        // it is never passed to a provider as a truncated rebuilt image.
        for table in [
            JOURNAL_TABLE_NAME,
            ordinary_claims::TABLE_NAME,
            ordinary_claims::RELEASE_TABLE_NAME,
            recovery_reserve::CHARGE_TABLE_NAME,
            recovery_reserve::TRANSITION_TABLE_NAME,
            recovery_reserve_binding::TABLE_NAME,
        ] {
            self.prestart_bounded_count(table, MAX_ROWS)?;
        }
        self.prestart_bounded_count(recording_chunks::TABLE_NAME, MAX_RECORDING_ROWS)?;
        self.prestart_bounded_count(chunk_claims::TABLE_NAME, 0)?;
        // These original audits may own arbitrary command JSON/aggregate maps. Until their
        // complete backing is budgeted, the narrow factory refuses before materialization.
        for table in [
            database_claims::BINDING_TABLE,
            database_claims::SLICE_TABLE,
            database_attempts::START_TABLE,
            database_attempts::OUTCOME_TABLE,
            FULL_PROGRESS_TABLE_NAME,
        ] {
            self.prestart_bounded_count(table, 0)?;
        }
        let valid: bool = self.connection.query_row(
            "SELECT COALESCE(MIN(typeof(operation_id)='text'
             AND length(CAST(operation_id AS BLOB))=36 AND typeof(sequence)='integer'
             AND typeof(record)='blob' AND length(record) BETWEEN 1 AND ?1),1)
             AND COALESCE(SUM(length(record)),0)<=?2 FROM operation_journal",
            params![MAX_RECORD_BYTES, MAX_TOTAL_RECORD_BYTES],
            |row| row.get(0),
        )?;
        if !valid {
            return Err(OperationJournalStoreError::RecordingPrestartResourceBound);
        }
        for (table, fields) in [
            (
                ordinary_claims::TABLE_NAME,
                &[
                    "claim_id",
                    "operation_id",
                    "volume_device",
                    "allocation_class",
                ][..],
            ),
            (
                ordinary_claims::RELEASE_TABLE_NAME,
                &["claim_id", "operation_id", "release_id"][..],
            ),
            (
                recording_chunks::TABLE_NAME,
                &["chunk_id", "operation_id"][..],
            ),
            (
                chunk_claims::TABLE_NAME,
                &[
                    "chunk_id",
                    "operation_id",
                    "claim_id",
                    "participant_id",
                    "output_key",
                    "volume_device",
                    "allocation_class",
                    "state",
                ][..],
            ),
            (
                recovery_reserve::CHARGE_TABLE_NAME,
                &[
                    "charge_id",
                    "volume_device",
                    "operation_id",
                    "purpose",
                    "state",
                ][..],
            ),
            (
                recovery_reserve::TRANSITION_TABLE_NAME,
                &["charge_id", "transition_id", "kind", "state"][..],
            ),
            (
                recovery_reserve_binding::TABLE_NAME,
                &[
                    "volume_device",
                    "profile_identity",
                    "root_identity",
                    "file_identity",
                    "namespace_name",
                ][..],
            ),
        ] {
            for field in fields {
                // SQLite length(TEXT) counts code points: count UTF-8 bytes instead. The
                // general semantic audit still validates exact IDs/classes and relationships.
                let bad: bool = self.connection.query_row(
                    &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE typeof({field})!='text' OR length(CAST({field} AS BLOB))>1024)"),
                    [], |row| row.get(0),
                )?;
                if bad {
                    return Err(OperationJournalStoreError::RecordingPrestartResourceBound);
                }
            }
        }
        for (table, field) in [
            (chunk_claims::TABLE_NAME, "issue_id"),
            (chunk_claims::TABLE_NAME, "release_id"),
            (recovery_reserve::CHARGE_TABLE_NAME, "active_volume_device"),
        ] {
            let bad: bool = self.connection.query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE {field} IS NOT NULL AND (typeof({field})!='text' OR length(CAST({field} AS BLOB))>1024))"),
                [], |row| row.get(0),
            )?;
            if bad {
                return Err(OperationJournalStoreError::RecordingPrestartResourceBound);
            }
        }
        // Existing captured immutable payloads need a larger decoded-value allowance. This
        // preparation envelope supports only the bounded reserved control rows.
        let captured: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM operation_journal_recording_chunks WHERE typeof(slot)!='integer' OR slot NOT BETWEEN 20000 AND 20003 OR typeof(record)!='blob' OR length(record) NOT BETWEEN 1 AND 1024)",
            [], |row| row.get(0),
        )?;
        if captured {
            return Err(OperationJournalStoreError::RecordingPrestartResourceBound);
        }
        // The general recording audit validates shape before copying immutable bytes.
        self.recording_retained_totals()?;
        Ok(())
    }

    fn prestart_bounded_count(
        &self,
        table: &str,
        maximum: i64,
    ) -> Result<(), OperationJournalStoreError> {
        let count: i64 = self.connection.query_row(
            &format!("SELECT COUNT(*) FROM (SELECT 1 FROM {table} LIMIT ?1)"),
            [maximum + 1],
            |row| row.get(0),
        )?;
        if count > maximum {
            return Err(OperationJournalStoreError::RecordingPrestartResourceBound);
        }
        Ok(())
    }

    fn prestart_decode_complete_record_inventory(&self) -> Result<(), OperationJournalStoreError> {
        let mut after = String::new();
        loop {
            let mut statement = self.connection.prepare(
                "SELECT operation_id FROM operation_journal WHERE operation_id>?1 ORDER BY operation_id LIMIT 8",
            )?;
            let keys = statement
                .query_map([&after], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            if keys.is_empty() {
                break;
            }
            for key in keys {
                let id = Uuid::parse_str(&key)
                    .map_err(|_| OperationJournalStoreError::RecordingPrestartResourceBound)?;
                if id.is_nil() || id.to_string() != key {
                    return Err(OperationJournalStoreError::RecordingPrestartResourceBound);
                }
                self.read_row(OperationId::from_uuid(id))?
                    .ok_or(OperationJournalStoreError::RecordingPrestartResourceBound)?;
                after = key;
            }
        }
        Ok(())
    }
}
