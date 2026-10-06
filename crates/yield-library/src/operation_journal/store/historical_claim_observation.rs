//! Borrowed ordinary-claim BOOL lookup in the original admission read transaction.
use super::super::codec::historical_observation;
use super::super::owner::InputPermit;
use super::*;

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
        let sql_bytes = historical_observation::FIXED_BYTES
            .checked_add(path_bytes)
            .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
        let sql = input.reserve_historical_scratch(sql_bytes)?;
        let mut scratch = None;
        let result = (|| {
            let (legacy, full) = self.operation_family_presence(operation_id)?;
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
            let mut statement = self.connection.prepare(
            "SELECT typeof(operation_id), typeof(sequence), sequence, typeof(record), length(record),
             CASE WHEN typeof(record) = 'blob' AND length(record) BETWEEN 0 AND ?2
                  THEN record ELSE NULL END
             FROM operation_journal WHERE operation_id = ?1"
        )?;
            let mut rows =
                statement.query(params![operation_id.to_string(), MAX_RECORD_BYTES as i64])?;
            let Some(row) = rows.next()? else {
                return Ok(false);
            };
            if row.get_ref(0)? != ValueRef::Text(b"text")
                || row.get_ref(1)? != ValueRef::Text(b"integer")
                || row.get_ref(3)? != ValueRef::Text(b"blob")
            {
                return Err(retained_error(
                    Some(operation_id),
                    "stored operation key, sequence or record has the wrong SQLite type",
                ));
            }
            let ValueRef::Integer(sequence) = row.get_ref(2)? else {
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
            self.ensure_sequence_unique(sequence)?;
            let ValueRef::Integer(length) = row.get_ref(4)? else {
                return Err(retained_error(
                    Some(operation_id),
                    "stored record length is unavailable",
                ));
            };
            let ValueRef::Blob(bytes) = row.get_ref(5)? else {
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
            drop(rows);
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
            input.retain_historical_error([Some(sql), parser, extra]);
        }
        result
    }
}
