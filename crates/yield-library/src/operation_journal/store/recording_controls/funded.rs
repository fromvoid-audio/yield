//! Same original control observations, with original-pool backing before ownership.
use super::*;
use crate::operation_journal::store::historical_claim_observation::borrowed_sql::Statement;
use crate::operation_journal::{codec::historical_observation::FIXED_BYTES, owner::InputPermit};
use rusqlite::types::ValueRef;
use std::{
    cell::RefCell,
    io::{self, Write},
    mem::size_of,
};

struct Count(usize);
impl Write for Count {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .ok_or(io::ErrorKind::OutOfMemory)?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn integer(row: &Statement<'_>, index: i32) -> Result<i64, OperationJournalStoreError> {
    match row.get_ref(index)? {
        ValueRef::Integer(value) => Ok(value),
        value => Err(rusqlite::Error::InvalidColumnType(
            index as usize,
            row.column_name(index)?.into(),
            value.data_type(),
        )
        .into()),
    }
}
fn text<'a>(row: &'a Statement<'_>, index: i32) -> Result<&'a str, OperationJournalStoreError> {
    match row.get_ref(index)? {
        ValueRef::Text(bytes) => {
            std::str::from_utf8(bytes).map_err(|error| rusqlite::Error::Utf8Error(error).into())
        }
        value => Err(rusqlite::Error::InvalidColumnType(
            index as usize,
            row.column_name(index)?.into(),
            value.data_type(),
        )
        .into()),
    }
}

pub(super) fn verify(
    store: &OperationJournalStore,
    proposal: &RecordingProposal,
    required: bool,
    input: &InputPermit,
) -> Result<bool, OperationJournalStoreError> {
    let working = input.reserve_historical_scratch(
        FIXED_BYTES
            + 3 * size_of::<Statement<'static>>()
            + 4 * size_of::<InputPermit>()
            + 2 * size_of::<RecordingControlReservation>(),
    )?;
    let native = RefCell::new(None);
    let mut encoding = None;
    let mut raw = None;
    let result = (|| {
        let mut key = [0; 36];
        let key = proposal
            .operation_id()
            .as_uuid()
            .hyphenated()
            .encode_lower(&mut key);
        let mut count = Statement::new(&store.connection, input, &native,
            c"SELECT COUNT(*) FROM (SELECT slot FROM operation_journal_recording_chunks WHERE operation_id=?1 AND slot>=?2 LIMIT 5)")?;
        count.bind_text(1, key)?;
        count.bind_integer(2, FIRST_CONTROL_SLOT)?;
        if !count.next()? {
            return Err(rusqlite::Error::QueryReturnedNoRows.into());
        }
        let n = integer(&count, 0)?;
        if n == 0 && !required {
            return Ok(false);
        }
        if n != CONTROL_ROWS {
            return Err(error(
                "recording has no complete reserved terminal inventory",
            ));
        }
        drop(count);
        // Stream the original borrowed proposal into a scalar byte counter. The
        // same serializer supplies the original proposal hash encoding; no JSON
        // Value tree, output Vec or cloned proposal is constructed by counting.
        let mut length = Count(0);
        serde_json::to_writer(&mut length, proposal).map_err(error)?;
        let encoded = length
            .0
            .checked_mul(2)
            .and_then(|n| n.checked_add(2 * CONTROL_BYTES))
            .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
        encoding = Some(input.reserve_historical_scratch(encoded)?);
        // Exact retained declaration and Frozen scalar graphs coexist across two rows.
        // Prepay their fixed bounded storage on the original pool before decoding either.
        let _history_backing = if store.recording_prestart_scope.get() {
            None // Preserve the existing strict empty-reservation pre-start debit.
        } else {
            Some(input.reserve_historical_scratch(4 * CONTROL_BYTES)?)
        };
        let mut history = ControlHistory::default();
        let mut previous: Option<i64> = None;
        for role in ControlRole::ALL {
            let expected = RecordingControlReservation::new(proposal, role).map_err(error)?;
            let mut row = Statement::new(&store.connection, input, &native,
                c"SELECT CASE WHEN typeof(chunk_id)='text' AND length(chunk_id)=36 THEN chunk_id ELSE NULL END,sequence,typeof(record),length(record) FROM operation_journal_recording_chunks WHERE operation_id=?1 AND slot=?2")?;
            row.bind_text(1, key)?;
            row.bind_integer(2, role.slot())?;
            if !row.next()? {
                return Err(error("reserved recording control absent"));
            }
            let mut expected_id = [0; 36];
            let expected_id = expected.id().hyphenated().encode_lower(&mut expected_id);
            let sequence = integer(&row, 1)?;
            if text(&row, 0)? != expected_id
                || sequence <= 0
                || text(&row, 2)? != "blob"
                || !(1..=CONTROL_BYTES as i64).contains(&integer(&row, 3)?)
            {
                return Err(error("unsupported recording control row shape/identity"));
            }
            if previous.is_some_and(|n| n.checked_add(1) != Some(sequence)) {
                return Err(error(
                    "reserved recording controls have conflicting sequence order",
                ));
            }
            previous = Some(sequence);
            drop(row);
            #[cfg(test)]
            super::super::recording_chunks::payload_read_boundary(
                super::super::recording_chunks::PayloadReader::Control,
                &store.connection,
            );
            let mut payload = Statement::new(&store.connection, input, &native,
                c"SELECT CASE WHEN typeof(record)='blob' AND length(record) BETWEEN 0 AND ?3 THEN record ELSE NULL END AS record,typeof(record) FROM operation_journal_recording_chunks WHERE operation_id=?1 AND slot=?2")?;
            payload.bind_text(1, key)?;
            payload.bind_integer(2, role.slot())?;
            payload.bind_integer(3, CONTROL_BYTES as i64)?;
            if !payload.next()? {
                return Err(rusqlite::Error::QueryReturnedNoRows.into());
            }
            let kind = text(&payload, 1)?;
            if kind != "blob" {
                return Err(super::super::recording_chunks::recording_payload_type_error(kind));
            }
            let bytes = match payload.get_ref(0)? {
                ValueRef::Blob(bytes) => bytes,
                _ if store.recording_prestart_scope.get() => {
                    return Err(OperationJournalStoreError::RecordingPrestartResourceBound);
                }
                _ => {
                    return Err(error(
                        "invalid recording observation: recording control exceeds reserved bound",
                    ));
                }
            };
            // Closed streaming control schema owns no field collections. Its
            // escaped-string parser scratch and the original serde error reason
            // (up to six debug-escape bytes, three simultaneous error strings,
            // geometric formatting growth) are prepaid.
            raw = Some(
                input.reserve_historical_scratch(
                    bytes
                        .len()
                        .checked_mul(2 + 3 * 2 * 6)
                        .and_then(|n| n.checked_add(FIXED_BYTES))
                        .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?,
                )?,
            );
            if store.recording_prestart_scope.get() {
                if bytes != expected.encode(proposal).map_err(error)? {
                    return Err(OperationJournalStoreError::RecordingPrestartResourceBound);
                }
            } else {
                finalizing_observation(bytes, proposal, role).map_err(error)?;
                history.observe(bytes, proposal, role, sequence)?;
            }
            // Borrowed payload and decoded scalar control die before raw credit
            // can be replaced for the next role.
            drop(payload);
            drop(raw.take());
        }
        let mut captures = Statement::new(&store.connection, input, &native,
            c"SELECT COUNT(*),MIN(sequence) FROM (SELECT sequence FROM operation_journal_recording_chunks WHERE operation_id=?1 AND slot NOT BETWEEN 20000 AND 20003 LIMIT 8193)")?;
        captures.bind_text(1, key)?;
        if !captures.next()? {
            return Err(rusqlite::Error::QueryReturnedNoRows.into());
        }
        if integer(&captures, 0)? > 8192 - CONTROL_ROWS {
            return Err(error(
                "retained recording capture exceeds its reserved inventory bound",
            ));
        }
        if let ValueRef::Integer(first) = captures.get_ref(1)? {
            if first <= previous.expect("four original control sequences") {
                return Err(error("recording capture precedes its terminal reservation"));
            }
        } else if captures.get_ref(1)? != ValueRef::Null {
            return Err(rusqlite::Error::InvalidColumnType(
                1,
                captures.column_name(1)?.into(),
                captures.get_ref(1)?.data_type(),
            )
            .into());
        }
        Ok(true)
    })();
    if result.is_err() {
        input.retain_historical_error(working, encoding, raw, native.into_inner());
    }
    result
}
