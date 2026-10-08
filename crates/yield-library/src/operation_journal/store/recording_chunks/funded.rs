//! Original recording observations prepaid on the same input, never capture grants.
use super::*;
use crate::operation_journal::store::{
    database_claims::{
        command_backing,
        scalar_projection::{self, UuidProjection},
    },
    historical_claim_observation::borrowed_sql::{Statement, scalar},
};
use crate::operation_journal::{codec::historical_observation::FIXED_BYTES, owner::InputPermit};
use rusqlite::types::ValueRef;
use std::{cell::RefCell, mem::size_of};

mod chunk;
mod scan;
pub(in crate::operation_journal::store) use chunk::{Chunk, read};
pub(in crate::operation_journal::store) use scan::validate;
#[cfg(test)]
thread_local! {
    pub(in crate::operation_journal::store) static PAGE_CELLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
fn checked(n: Option<usize>) -> Result<usize, OperationJournalStoreError> {
    n.ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)
}
pub(in crate::operation_journal::store) fn working_bytes(
    store: &OperationJournalStore,
) -> Result<usize, OperationJournalStoreError> {
    // The original duplicate-sequence audit retains its path and formats it at
    // the caller. Derive both native path storage and lossy/display growth.
    checked(
        store
            .database_path
            .as_os_str()
            .as_encoded_bytes()
            .len()
            .checked_mul(7)
            .and_then(|n| {
                n.checked_add(
                    FIXED_BYTES
                        + command_backing::shape_working_bytes()
                        + 4 * size_of::<Statement<'static>>()
                        + size_of::<Chunk>()
                        + 4 * size_of::<InputPermit>()
                        + 8 * (36 + size_of::<String>())
                        + 4096,
                )
            }),
    )
}
fn key(row: &Statement<'_>, index: i32) -> Result<UuidProjection, OperationJournalStoreError> {
    let value = row.get_ref(index)?;
    if let Some(key) =
        scalar_projection::uuid_value(value, index as usize, row.column_name(index)?)?
    {
        return Ok(key);
    }
    // A required String NULL must keep its original driver conversion error.
    scalar::<String>(row, index)
        .map(|_| unreachable!("nullable key fallback only succeeds for NULL"))
}
fn text<'a>(row: &'a Statement<'_>, index: i32) -> Result<&'a str, OperationJournalStoreError> {
    if let ValueRef::Text(bytes) = row.get_ref(index)? {
        if let Ok(text) = std::str::from_utf8(bytes) {
            return Ok(text);
        }
    }
    scalar::<String>(row, index)
        .map(|_| unreachable!("valid TEXT returned through borrowed branch"))
}
fn totals(
    store: &OperationJournalStore,
    input: &InputPermit,
    native: &RefCell<Option<InputPermit>>,
) -> Result<(), OperationJournalStoreError> {
    let mut row=Statement::new(&store.connection,input,native,c"SELECT COUNT(*),COALESCE(SUM(CASE WHEN slot BETWEEN 20000 AND 20003 THEN 4096 ELSE length(record) END),0),
             COALESCE(MIN(CASE WHEN typeof(chunk_id)='text' AND length(chunk_id)=36
              AND typeof(operation_id)='text' AND length(operation_id)=36
              AND typeof(sequence)='integer' AND sequence>0 AND typeof(slot)='integer'
              AND (slot BETWEEN 0 AND 8175 OR slot BETWEEN 10000 AND 10015 OR slot BETWEEN 20000 AND 20003)
              AND typeof(record)='blob' AND length(record) BETWEEN 1 AND 131072
              AND (slot NOT BETWEEN 20000 AND 20003 OR length(record)<=4096)
              THEN 1 ELSE 0 END),1)
             FROM (SELECT chunk_id,operation_id,slot,sequence,record FROM operation_journal_recording_chunks LIMIT 8193)")?;
    if !row.next()? {
        return Err(rusqlite::Error::QueryReturnedNoRows.into());
    }
    let rows = scalar::<i64>(&row, 0)?;
    let bytes = scalar::<i64>(&row, 1)?;
    let valid = scalar::<i64>(&row, 2)?;
    if valid != 1 || rows > MAX_ROWS || bytes > MAX_RETAINED_BYTES {
        return Err(error(
            "unsupported/corrupt or over-limit retained recording inventory",
        ));
    }
    Ok(())
}
fn sequence_unique(
    store: &OperationJournalStore,
    sequence: i64,
    input: &InputPermit,
    native: &RefCell<Option<InputPermit>>,
) -> Result<(), OperationJournalStoreError> {
    let mut row = Statement::new(
        &store.connection,
        input,
        native,
        c"SELECT COUNT(*) FROM (
                 SELECT sequence FROM operation_journal
                 WHERE typeof(sequence) = 'integer' AND sequence = ?1
                 UNION ALL
                 SELECT sequence FROM operation_journal_full_progress
                 WHERE typeof(sequence) = 'integer' AND sequence = ?1
                 UNION ALL
                 SELECT sequence FROM operation_journal_staging_checkpoints
                 WHERE typeof(sequence) = 'integer' AND sequence = ?1
                 UNION ALL
                 SELECT sequence FROM operation_journal_chunk_staging
                 WHERE typeof(sequence) = 'integer' AND sequence = ?1
                 UNION ALL
                 SELECT sequence FROM operation_journal_recording_chunks
                 WHERE typeof(sequence) = 'integer' AND sequence = ?1
             )",
    )?;
    row.bind_integer(1, sequence)?;
    if !row.next()? {
        return Err(rusqlite::Error::QueryReturnedNoRows.into());
    }
    if scalar::<i64>(&row, 0)? > 1 {
        return Err(super::super::audit_error(
            &store.database_path,
            format!("duplicate journal-observation sequence {sequence}"),
        ));
    }
    Ok(())
}
