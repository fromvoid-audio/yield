//! Borrowed ordinary-claim BOOL lookup in the original admission read transaction.
use super::super::codec::historical_observation;
use super::super::owner::InputPermit;
use super::*;
use std::cell::RefCell;
mod borrowed_sql;
use borrowed_sql::Statement;

impl OperationJournalStore {
    #[cfg(any(test, feature = "test-support"))]
    /// Observe the real SQLite lookup with an isolated instance of the original heap pool.
    /// This component fixture installs no worker, physical capacity, or effect authority.
    /// Production admission uses the actual queued command's pool, never this fixture.
    pub fn observe_historical_claim_for_test(
        &self,
        operation_id: OperationId,
    ) -> Result<bool, OperationJournalStoreError> {
        let input = InputPermit::historical_test_input(1);
        self.connection.execute_batch("BEGIN")?;
        let result = self.observe_claim_operation(operation_id, &input);
        self.connection.execute_batch("ROLLBACK")?;
        result
    }

    pub(super) fn observe_claim_operation(
        &self,
        operation_id: OperationId,
        input: &InputPermit,
    ) -> Result<bool, OperationJournalStoreError> {
        // Fund fixed Rust statement/binding/error backing before preparing SQL. Native
        // SQLite allocations are not represented as a Rust allocator/RSS bound.
        // audit_error clones the original path; debit its exact encoded byte length first.
        #[cfg(windows)]
        let path_bytes = {
            use std::os::windows::ffi::OsStrExt;
            self.database_path
                .as_os_str()
                .encode_wide()
                .count()
                .checked_mul(2)
                .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?
        };
        #[cfg(not(windows))]
        let path_bytes = self.database_path.as_os_str().as_encoded_bytes().len();
        // Display may replace each invalid native byte with three UTF-8 bytes.
        // Both original owned reason and the consumer's formatted diagnostic overlap.
        let prefix =
            "ordinary claim  operation  requires audit: operation journal at  requires audit: "
                .len()
                + 2 * 36;
        let display_bytes = path_bytes
            .checked_mul(3)
            .and_then(|n| n.checked_add(historical_observation::FIXED_BYTES))
            .and_then(|n| n.checked_add(prefix))
            .and_then(|n| n.checked_mul(2))
            .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
        let sql_bytes = historical_observation::FIXED_BYTES
            .checked_add(path_bytes)
            .and_then(|n| n.checked_add(display_bytes))
            .and_then(|n| {
                n.checked_add(
                    3 * std::mem::size_of::<Statement<'_>>()
                        + std::mem::size_of::<RefCell<Option<InputPermit>>>(),
                )
            })
            .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
        let sql = input.reserve_historical_scratch(sql_bytes)?;
        let mut scratch = None;
        let native_error = RefCell::new(None);
        let result = (|| {
            let mut key_buffer = [0; 36];
            let key = operation_id
                .as_uuid()
                .hyphenated()
                .encode_lower(&mut key_buffer);
            let mut family_present = [false; 2];
            for (index, query) in [
                c"SELECT EXISTS(SELECT 1 FROM operation_journal WHERE operation_id = ?1)",
                c"SELECT EXISTS(SELECT 1 FROM operation_journal_full_progress WHERE operation_id = ?1)",
            ].into_iter().enumerate() {
                let mut family = Statement::new(&self.connection, input, &native_error, query)?;
                family.bind_text(1, key)?;
                if !family.next()? { return Err(rusqlite::Error::QueryReturnedNoRows.into()); }
                let ValueRef::Integer(present) = family.get_ref(0)? else { unreachable!("SQLite EXISTS is an integer") };
                family_present[index] = present != 0;
            }
            let [legacy, full] = family_present;
            if legacy && full {
                return Err(audit_error(
                    &self.database_path,
                    "operation appears in both journal families",
                ));
            }
            if !legacy {
                return Ok(false);
            }
            // Declaration order also covers panic/early-return: rows, statement, then
            // the original scratch debit. No borrowed Native row outlives its credit.
            let mut statement = Statement::new(&self.connection, input, &native_error,
                c"SELECT typeof(operation_id), typeof(sequence), sequence, typeof(record), length(record),
                 CASE WHEN typeof(record) = 'blob' AND length(record) BETWEEN 0 AND ?2 THEN record ELSE NULL END
                 FROM operation_journal WHERE operation_id = ?1")?;
            statement.bind_text(1, key)?;
            statement.bind_integer(2, MAX_RECORD_BYTES as i64)?;
            if !statement.next()? {
                return Ok(false);
            }
            if statement.get_ref(0)? != ValueRef::Text(b"text")
                || statement.get_ref(1)? != ValueRef::Text(b"integer")
                || statement.get_ref(3)? != ValueRef::Text(b"blob")
            {
                return Err(retained_error(
                    Some(operation_id),
                    "stored operation key, sequence or record has the wrong SQLite type",
                ));
            }
            let ValueRef::Integer(sequence) = statement.get_ref(2)? else {
                return Err(retained_error(
                    Some(operation_id),
                    "stored sequence is not an integer",
                ));
            };
            if sequence <= 0 {
                return Err(retained_error(
                    Some(operation_id),
                    "stored sequence is not positive",
                ));
            }
            {
                let mut unique = Statement::new(&self.connection, input, &native_error,
                    c"SELECT COUNT(*) FROM (
                       SELECT sequence FROM operation_journal WHERE typeof(sequence) = 'integer' AND sequence = ?1
                       UNION ALL SELECT sequence FROM operation_journal_full_progress WHERE typeof(sequence) = 'integer' AND sequence = ?1
                       UNION ALL SELECT sequence FROM operation_journal_staging_checkpoints WHERE typeof(sequence) = 'integer' AND sequence = ?1
                       UNION ALL SELECT sequence FROM operation_journal_chunk_staging WHERE typeof(sequence) = 'integer' AND sequence = ?1
                       UNION ALL SELECT sequence FROM operation_journal_recording_chunks WHERE typeof(sequence) = 'integer' AND sequence = ?1)")?;
                unique.bind_integer(1, sequence)?;
                if !unique.next()? {
                    return Err(rusqlite::Error::QueryReturnedNoRows.into());
                }
                let ValueRef::Integer(count) = unique.get_ref(0)? else {
                    unreachable!("SQLite COUNT is an integer")
                };
                if count > 1 {
                    return Err(audit_error(
                        &self.database_path,
                        format!("duplicate journal-observation sequence {sequence}"),
                    ));
                }
            }
            let ValueRef::Integer(length) = statement.get_ref(4)? else {
                return Err(retained_error(
                    Some(operation_id),
                    "stored record length is unavailable",
                ));
            };
            let ValueRef::Blob(bytes) = statement.get_ref(5)? else {
                return Err(retained_error(
                    Some(operation_id),
                    "stored record is not a bounded SQLite BLOB",
                ));
            };
            if !(0..=MAX_RECORD_BYTES as i64).contains(&length) || length as usize != bytes.len() {
                return Err(retained_error(
                    Some(operation_id),
                    "stored record length changed or exceeds its bound",
                ));
            }
            scratch = Some(historical_observation::Scratch::new(input, bytes.len())?);
            let observed =
                historical_observation::observe(bytes, scratch.as_mut().expect("funded scratch"));
            // End the SQLite borrow before releasing its per-row scratch or falling through
            // to the unchanged other-family reader. No historical payload is copied here.
            drop(statement);
            let observation = observed?;
            let Some(record) = observation.as_ref() else {
                drop(observation);
                drop(scratch.take());
                return self.read_row(operation_id).map(|row| row.is_some());
            };
            if record.operation_id != operation_id {
                return Err(retained_error(
                    Some(operation_id),
                    "record identity does not match its SQLite key",
                ));
            }
            match classify_record(record) {
                RecordConsistency::Consistent => Ok(true),
                RecordConsistency::LegacyMissingPublicationEvidence => Err(retained_error(
                    Some(operation_id),
                    "legacy record is retained without publication evidence",
                )),
                RecordConsistency::Invalid(reason) => {
                    Err(retained_error(Some(operation_id), reason))
                }
            }
        })();
        if result.is_err() {
            let [parser, extra] = scratch
                .take()
                .map(|scratch| scratch.into_permits())
                .unwrap_or([None, None]);
            input.retain_historical_error(sql, parser, extra, native_error.into_inner());
        }
        result
    }
}
