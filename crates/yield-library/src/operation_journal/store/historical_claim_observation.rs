//! Borrowed ordinary-claim BOOL lookup in the original admission read transaction.
use super::super::codec::historical_observation;
use super::super::owner::InputPermit;
use super::*;
use std::cell::RefCell;
pub(super) mod borrowed_sql;
use borrowed_sql::Statement;

#[cfg(test)]
thread_local! {
    pub(super) static GENERAL_FUND_ATTEMPTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(super) static GENERAL_DECODE_ENTRIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl OperationJournalStore {
    #[cfg(any(test, feature = "test-support"))]
    /// Replace one raw historical-row fixture on this original Store connection.
    /// No worker, capacity, effect or physical backend authority is installed.
    pub fn replace_historical_row_fixture_for_test(
        &self,
        operation: OperationId,
        sequence: i64,
        wire: &[u8],
    ) -> Result<(), OperationJournalStoreError> {
        self.connection.execute(
            "INSERT OR REPLACE INTO operation_journal VALUES (?1,?2,?3)",
            params![operation.to_string(), sequence, wire],
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub(in crate::operation_journal) fn install_historical_claim_fixture(
        &self,
        operation: OperationId,
        sequence: i64,
        wire: &[u8],
        claim: Uuid,
    ) -> Result<(), OperationJournalStoreError> {
        // Test corruption uses the SAME genuine Store connection, never a competing
        // opener or relaxed locking mode. This grants no production effect authority.
        self.replace_historical_row_fixture_for_test(operation, sequence, wire)?;
        self.connection.execute("INSERT INTO operation_journal_ordinary_claim VALUES (?1,?2,'1','DestinationStaging',1)", params![claim.to_string(), operation.to_string()])?;
        Ok(())
    }

    #[cfg(any(test, feature = "test-support"))]
    /// Scalar allocation diagnostic for a borrowed SQLite-error conversion fixture.
    /// This issues no SQL, worker, capacity or effect authority. The production
    /// observer uses this same funded copy routine on actual borrowed sqlite3_errmsg.
    pub fn historical_error_conversion_for_test(
        &self,
        nul_terminated: &[u8],
    ) -> Result<(usize, usize), OperationJournalStoreError> {
        let input = InputPermit::historical_test_input(1);
        let sql = input.reserve_historical_scratch(historical_observation::FIXED_BYTES)?;
        let message = std::ffi::CStr::from_bytes_with_nul(nul_terminated)
            .map_err(|_| retained_error(None, "error fixture is not one NUL-terminated message"))?;
        let (owned, credit) = borrowed_sql::copy_error_message(&input, message)?;
        let length = owned.len();
        let bytes = credit.historical_test_funded_bytes();
        let error = OperationJournalStoreError::Sql(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
            Some(owned),
        ));
        let formatted = format!(
            "ordinary claim {} operation {} requires audit: {error}",
            Uuid::nil(),
            Uuid::nil()
        );
        let OperationJournalStoreError::Sql(rusqlite::Error::SqliteFailure(_, Some(ref copied))) =
            error
        else {
            unreachable!("fixture constructs typed SQLite error")
        };
        assert!(formatted.ends_with(copied));
        drop(formatted);
        drop(error);
        drop(credit);
        drop(sql);
        Ok((length, bytes))
    }

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

    #[cfg(any(test, feature = "test-support"))]
    /// Borrow this exact original diagnostic pool for allocation/lifetime observation.
    /// The caller owns the read transaction; this creates no pool, authority or grant.
    pub fn observe_historical_claim_with_funding_for_test(
        &self,
        operation_id: OperationId,
        funding: &super::super::owner::JournalNamespaceFundingProbe,
    ) -> Result<bool, OperationJournalStoreError> {
        self.observe_claim_operation(operation_id, &funding.input)
    }

    #[cfg(any(test, feature = "test-support"))]
    /// Compare the exact original complete record while its producer loan remains
    /// live. Only scalar comparison evidence escapes; no authority is installed.
    pub fn complete_historical_matches_with_funding_for_test(
        &self,
        expected: &StoredOperationRecord,
        expected_sequence: i64,
        funding: &super::super::owner::JournalNamespaceFundingProbe,
    ) -> Result<bool, OperationJournalStoreError> {
        let mut matched = false;
        let present = self.inspect_complete_claim_operation(
            expected.operation_id,
            &funding.input,
            |record, sequence| {
                matched = record == expected && sequence == expected_sequence;
                Ok(())
            },
        )?;
        Ok(present && matched)
    }

    pub(super) fn observe_claim_operation(
        &self,
        operation_id: OperationId,
        input: &InputPermit,
    ) -> Result<bool, OperationJournalStoreError> {
        self.inspect_claim_operation(operation_id, input, false, |_, _| Ok(()))
    }

    /// Inspect the exact complete original record only while its own backing is
    /// retained. The visitor returns no record, clone, lease or effect authority.
    pub(super) fn inspect_complete_claim_operation(
        &self,
        operation_id: OperationId,
        input: &InputPermit,
        inspect: impl FnOnce(&StoredOperationRecord, i64) -> Result<(), OperationJournalStoreError>,
    ) -> Result<bool, OperationJournalStoreError> {
        self.inspect_claim_operation(operation_id, input, true, inspect)
    }

    fn inspect_claim_operation(
        &self,
        operation_id: OperationId,
        input: &InputPermit,
        complete: bool,
        inspect: impl FnOnce(&StoredOperationRecord, i64) -> Result<(), OperationJournalStoreError>,
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
            let mut observation =
                historical_observation::observe(bytes, scratch.as_mut().expect("funded scratch"))?;
            if complete || observation.is_none() {
                // A V1/V2 BOOL projection omits opaque payload. Its original
                // graph must die before replacing its loan with complete backing.
                drop(observation.take());
                // The original general codec consumes this borrowed SQLite cell only after
                // complete structure-derived backing. No second SQL read or owned BLOB copy.
                #[cfg(test)]
                GENERAL_FUND_ATTEMPTS.with(|attempts| attempts.set(attempts.get() + 1));
                scratch
                    .as_mut()
                    .expect("funded scratch")
                    .fund_general(bytes)?;
                #[cfg(test)]
                GENERAL_DECODE_ENTRIES.with(|entries| entries.set(entries.get() + 1));
                let record = if self.recording_prestart_scope.get() {
                    super::super::codec::decode_prestart_recording(
                        bytes,
                        recording_prestart_budget::JSON_LIMITS,
                    )
                    .map_err(|error| match error {
                        super::super::codec::BoundedDecodeError::LimitExceeded { .. } => {
                            self.recording_prestart_resource_refused.set(true);
                            OperationJournalStoreError::RecordingPrestartResourceBound
                        }
                        super::super::codec::BoundedDecodeError::Decode(error) => {
                            retained_decode_error(operation_id, error)
                        }
                    })?
                    .ok_or_else(|| {
                        self.recording_prestart_resource_refused.set(true);
                        OperationJournalStoreError::RecordingPrestartResourceBound
                    })?
                } else {
                    decode(bytes).map_err(|error| retained_decode_error(operation_id, error))?
                };
                observation = Some(record);
            }
            // End the original cell borrow with the decoded observation still prepaid.
            drop(statement);
            let record = observation
                .as_ref()
                .expect("projected or fully decoded record");
            if record.operation_id != operation_id {
                return Err(retained_error(
                    Some(operation_id),
                    "record identity does not match its SQLite key",
                ));
            }
            match classify_record(record) {
                RecordConsistency::Consistent => {
                    inspect(record, sequence)?;
                    Ok(true)
                }
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
