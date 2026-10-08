//! Original same-transaction four-row recording reservation with prepaid ownership.
use super::*;
use crate::operation_journal::store::{
    database_claims::command_backing, historical_claim_observation::borrowed_sql::scalar,
    initial_settlement_backing, recording_chunks,
};
use crate::operation_journal::{StoredOperationRecord, owner::InputPermit};
use std::{cell::RefCell, mem::size_of};

pub(in crate::operation_journal::store) fn reserve(
    store: &OperationJournalStore,
    record: &StoredOperationRecord,
    input: &InputPermit,
) -> Result<(), OperationJournalStoreError> {
    let Some(UniversalCommandArgs::Recording(proposal)) = record.universal_command_args() else {
        return Ok(());
    };
    let working = input.reserve_historical_scratch(
        command_backing::shape_working_bytes()
            + size_of::<RecordingControlReservation>()
            + 4096
            + 4 * size_of::<InputPermit>(),
    )?;
    let mut producer = None;
    let native = RefCell::new(None);
    let result = (|| {
        if store.connection.is_autocommit()
            || !initial_settlement_backing::matches_inside(store, record, input)?
        {
            return Err(error(
                "recording reservation requires the exact initial intent transaction",
            ));
        }
        let count = initial_settlement_backing::statement(&store.connection, input,
            c"SELECT COUNT(*) FROM (SELECT slot FROM operation_journal_recording_chunks WHERE operation_id=?1 LIMIT 5)", |row| {
            row.bind_text(1, &proposal.operation_id().to_string())?;
            if !row.next()? { return Err(rusqlite::Error::QueryReturnedNoRows.into()); }
            scalar::<i64>(row,0)
        })?;
        if count != 0 {
            store.verify_recording_reservation_with_input(proposal, true, Some(input))?;
            return Ok(());
        }
        let wire = command_backing::serialized_length(proposal)?;
        let bytes = wire
            .max(4096)
            .checked_mul(4)
            .and_then(|n| n.checked_add(2 * CONTROL_BYTES))
            .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
        producer = Some(input.reserve_historical_scratch(bytes)?);
        reservation_metadata_bound(proposal).map_err(error)?;
        let (rows, bytes) = recording_chunks::funded::totals(store, input, &native)?;
        if rows.checked_add(CONTROL_ROWS).is_none_or(|n| n > 8192)
            || bytes.checked_add(RESERVED_BYTES as i64).is_none_or(|n| n > crate::operation_journal::recording_commands::MAX_RECORDING_RETAINED_ENCODED_BYTES as i64) {
            return Err(error("recording retention leaves no terminal metadata reservation"));
        }
        for role in ControlRole::ALL {
            let value = RecordingControlReservation::new(proposal, role).map_err(error)?;
            let encoded = value.encode(proposal).map_err(error)?;
            let sequence = initial_settlement_backing::next_sequence(store, input)?;
            initial_settlement_backing::statement(&store.connection, input,
                c"INSERT INTO operation_journal_recording_chunks(chunk_id,operation_id,slot,sequence,record) VALUES(?1,?2,?3,?4,?5)", |row| {
                row.bind_text(1, &value.id().to_string())?;
                row.bind_text(2, &proposal.operation_id().to_string())?;
                row.bind_integer(3, value.slot())?;
                row.bind_integer(4, sequence)?;
                row.bind_blob(5, &encoded)?;
                row.next().map(|_| ())
            })?;
        }
        store.verify_recording_reservation_with_input(proposal, true, Some(input))?;
        Ok(())
    })();
    if result.is_err() {
        input.retain_historical_error(working, producer, None, native.into_inner());
    }
    result
}
