//! Original write controls on the same connection and original input pool.
use super::historical_claim_observation::borrowed_sql::Statement;
use super::*;
use crate::operation_journal::owner::InputPermit;
use std::{cell::RefCell, ffi::CStr};

pub(super) fn control(
    connection: &Connection,
    input: Option<&InputPermit>,
    sql: &'static CStr,
) -> Result<(), OperationJournalStoreError> {
    let Some(input) = input else {
        return connection
            .execute_batch(sql.to_str().expect("static original SQL"))
            .map_err(Into::into);
    };
    let native = RefCell::new(None);
    // The caller holds the fixed write/error envelope through the transaction.
    // Even mandatory rollback under pressure prepares/steps before copying any
    // native message. Its failure cannot replace a primary retained native loan.
    let result = (|| {
        let mut statement = Statement::new(connection, input, &native, sql)?;
        statement.next().map(|_| ())
    })();
    if let Some(native) = native.into_inner() {
        input.retain_historical_error(native, None, None, None);
    }
    result
}

pub(super) fn statement<T>(
    connection: &Connection,
    input: &InputPermit,
    sql: &'static CStr,
    observe: impl FnOnce(&mut Statement<'_>) -> Result<T, OperationJournalStoreError>,
) -> Result<T, OperationJournalStoreError> {
    let native = RefCell::new(None);
    let result = (|| {
        let mut row = Statement::new(connection, input, &native, sql)?;
        observe(&mut row)
    })();
    if let Some(native) = native.into_inner() {
        input.retain_historical_error(native, None, None, None);
    }
    result
}

pub(super) fn family_presence(
    store: &OperationJournalStore,
    operation: OperationId,
    input: &InputPermit,
) -> Result<(bool, bool), OperationJournalStoreError> {
    use super::historical_claim_observation::borrowed_sql::scalar;
    let mut buffer = [0; 36];
    let key = operation.as_uuid().hyphenated().encode_lower(&mut buffer);
    let read = |sql| {
        statement(&store.connection, input, sql, |row| {
            row.bind_text(1, key)?;
            if !row.next()? {
                return Err(rusqlite::Error::QueryReturnedNoRows.into());
            }
            scalar::<bool>(row, 0)
        })
    };
    Ok((
        read(c"SELECT EXISTS(SELECT 1 FROM operation_journal WHERE operation_id = ?1)")?,
        read(
            c"SELECT EXISTS(SELECT 1 FROM operation_journal_full_progress WHERE operation_id = ?1)",
        )?,
    ))
}

pub(super) fn next_sequence(
    store: &OperationJournalStore,
    input: &InputPermit,
) -> Result<i64, OperationJournalStoreError> {
    use super::historical_claim_observation::borrowed_sql::scalar;
    let previous = statement(
        &store.connection,
        input,
        c"SELECT MAX(sequence) FROM (
                 SELECT sequence FROM operation_journal
                 UNION ALL
                 SELECT sequence FROM operation_journal_full_progress
                 UNION ALL
                 SELECT sequence FROM operation_journal_staging_checkpoints
                 UNION ALL
                 SELECT sequence FROM operation_journal_chunk_staging
                 UNION ALL
                 SELECT sequence FROM operation_journal_recording_chunks
             )",
        |row| {
            if !row.next()? {
                return Err(rusqlite::Error::QueryReturnedNoRows.into());
            }
            scalar::<Option<i64>>(row, 0)
        },
    )?;
    if previous.is_some_and(|n| n <= 0) {
        return Err(audit_error(
            &store.database_path,
            "stored journal-observation sequence is not positive",
        ));
    }
    previous
        .unwrap_or(0)
        .checked_add(1)
        .ok_or(OperationJournalStoreError::SequenceExhausted)
}

pub(super) fn insert_row(
    store: &OperationJournalStore,
    operation: OperationId,
    sequence: i64,
    encoded: &[u8],
    input: &InputPermit,
) -> Result<(), OperationJournalStoreError> {
    let mut buffer = [0; 36];
    let key = operation.as_uuid().hyphenated().encode_lower(&mut buffer);
    statement(
        &store.connection,
        input,
        c"INSERT INTO operation_journal (operation_id, sequence, record)
             VALUES (?1, ?2, ?3)",
        |row| {
            row.bind_text(1, key)?;
            row.bind_integer(2, sequence)?;
            row.bind_blob(3, encoded)?;
            row.next().map(|_| ())
        },
    )
}

pub(super) struct Scope<'a> {
    input: &'a InputPermit,
    working: Option<InputPermit>,
    producer: Option<InputPermit>,
}
impl<'a> Scope<'a> {
    pub(super) fn new(
        store: &OperationJournalStore,
        record: &StoredOperationRecord,
        claims: &AdmissionClaimInventory,
        input: &'a InputPermit,
    ) -> Result<Self, OperationJournalStoreError> {
        use super::database_claims::command_backing;
        use crate::durable_capacity::VolumeIdentity;
        use crate::operation_journal::{
            codec::historical_observation::FIXED_BYTES,
            full_capacity::{FullCapacityAllocation, FullCapacityAllocationClass},
        };
        use std::mem::size_of;
        let node = 12 * size_of::<((VolumeIdentity, FullCapacityAllocationClass), u64)>()
            + 16 * size_of::<usize>();
        let rows = record
            .full_capacity_plan
            .as_ref()
            .map_or(0, |p| p.allocations().len())
            .checked_add(record.capacity_plan.as_ref().map_or(0, |p| p.volumes.len()))
            .and_then(|n| n.checked_add(claims.claims().len()))
            .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
        let bytes = rows
            .checked_mul(3 * node + 2 * size_of::<FullCapacityAllocation>())
            .and_then(|n| {
                store
                    .database_path
                    .as_os_str()
                    .as_encoded_bytes()
                    .len()
                    .checked_mul(17)
                    .and_then(|p| n.checked_add(p))
            })
            .and_then(|n| {
                n.checked_add(
                    FIXED_BYTES
                        + command_backing::shape_working_bytes()
                        + 2 * size_of::<JournalInsertOutcome>()
                        + size_of::<InitialClaimSettlementError>()
                        + 4 * size_of::<InputPermit>()
                        + 2 * size_of::<Statement<'static>>()
                        + 3 * ordinary_claims::MAX_PAGE_ROWS
                            * size_of::<ordinary_claims::OrdinaryClaimRecord>()
                        + 4096,
                )
            })
            .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
        let mut scope = Self {
            input,
            working: Some(input.reserve_historical_scratch(bytes)?),
            producer: None,
        };
        let result = (|| {
            let shape = command_backing::settlement_shape(record)?;
            // Original validation and encoding: family clone/parser graphs,
            // sequential validators, one retained output and proposal hash output.
            let bytes = shape
                .cloned
                .checked_mul(3)
                .and_then(|n| shape.graph.checked_mul(3).and_then(|g| n.checked_add(g)))
                .and_then(|n| {
                    shape
                        .output
                        .max(4096)
                        .checked_mul(6)
                        .and_then(|o| n.checked_add(o))
                })
                .and_then(|n| n.checked_add(shape.reasons))
                .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
            scope.producer = Some(input.reserve_historical_scratch(bytes)?);
            Ok(())
        })();
        if let Err(error) = result {
            scope.finish(true);
            return Err(error);
        }
        Ok(scope)
    }
    pub(super) fn finish(mut self, selected: bool) {
        if selected {
            self.input.retain_historical_error(
                self.working.take().expect("original working loan"),
                self.producer.take(),
                None,
                None,
            );
        }
    }
}

pub(super) fn matches_inside(
    store: &OperationJournalStore,
    expected: &StoredOperationRecord,
    input: &InputPermit,
) -> Result<bool, OperationJournalStoreError> {
    let mut identical = false;
    let present =
        store.inspect_complete_claim_operation(expected.operation_id, input, |record, _| {
            identical = record == expected;
            Ok(())
        })?;
    Ok(present && identical)
}
/// Exact original encoding already paid by the enclosing initial-write scope.
/// This only compares a just-inserted recording intent; no stored row is used
/// as authority and no historical decoder or replacement BLOB is constructed.
pub(super) fn matches_encoded_inside(
    store: &OperationJournalStore,
    operation: OperationId,
    encoded: &[u8],
    input: &InputPermit,
) -> Result<bool, OperationJournalStoreError> {
    let (legacy, full) = family_presence(store, operation, input)?;
    if legacy && full {
        return Err(audit_error(
            &store.database_path,
            "operation appears in both journal families",
        ));
    }
    if !legacy {
        return Ok(false);
    }
    statement(&store.connection, input,
        c"SELECT typeof(operation_id), typeof(sequence), sequence, typeof(record), length(record),
          CASE WHEN typeof(record) = 'blob' AND length(record) BETWEEN 0 AND ?2 THEN record ELSE NULL END
          FROM operation_journal WHERE operation_id = ?1", |row| {
        row.bind_text(1, &operation.to_string())?;
        row.bind_integer(2, MAX_RECORD_BYTES as i64)?;
        if !row.next()? { return Ok(false); }
        if row.get_ref(0)? != ValueRef::Text(b"text") || row.get_ref(1)? != ValueRef::Text(b"integer") || row.get_ref(3)? != ValueRef::Text(b"blob") {
            return Err(retained_error(Some(operation), "stored operation key, sequence or record has the wrong SQLite type"));
        }
        let ValueRef::Integer(sequence) = row.get_ref(2)? else { unreachable!("checked SQLite INTEGER"); };
        if sequence <= 0 { return Err(retained_error(Some(operation), "stored sequence is not positive")); }
        statement(&store.connection, input,
            c"SELECT COUNT(*) FROM (
             SELECT sequence FROM operation_journal WHERE typeof(sequence) = 'integer' AND sequence = ?1
             UNION ALL SELECT sequence FROM operation_journal_full_progress WHERE typeof(sequence) = 'integer' AND sequence = ?1
             UNION ALL SELECT sequence FROM operation_journal_staging_checkpoints WHERE typeof(sequence) = 'integer' AND sequence = ?1
             UNION ALL SELECT sequence FROM operation_journal_chunk_staging WHERE typeof(sequence) = 'integer' AND sequence = ?1
             UNION ALL SELECT sequence FROM operation_journal_recording_chunks WHERE typeof(sequence) = 'integer' AND sequence = ?1)", |count| {
                count.bind_integer(1, sequence)?;
                if !count.next()? { return Err(rusqlite::Error::QueryReturnedNoRows.into()); }
                if super::historical_claim_observation::borrowed_sql::scalar::<i64>(count, 0)? > 1 {
                    return Err(audit_error(&store.database_path, format!("duplicate journal-observation sequence {sequence}")));
                }
                Ok(())
            })?;
        let ValueRef::Blob(bytes) = row.get_ref(5)? else { return Err(retained_error(Some(operation), "stored record is not a bounded SQLite BLOB")); };
        if row.get_ref(4)? != ValueRef::Integer(bytes.len() as i64) {
            return Err(retained_error(Some(operation), "stored record length changed or exceeds its bound"));
        }
        Ok(bytes == encoded)
    })
}

pub(super) fn readback_matches(
    store: &OperationJournalStore,
    expected: &StoredOperationRecord,
    encoded: &[u8],
    input: &InputPermit,
) -> Result<bool, OperationJournalStoreError> {
    store.validate_namespace_with_input(Some(input), false)?;
    let identical = if matches!(
        expected.universal_command_args(),
        Some(crate::operation_journal::coordinator::UniversalCommandArgs::Recording(_))
    ) {
        matches_encoded_inside(store, expected.operation_id, encoded, input)?
    } else {
        matches_inside(store, expected, input)?
    };
    store.validate_namespace_with_input(Some(input), false)?;
    Ok(identical)
}
