//! Internal persistence for the bounded full-progress observation envelope.
//!
//! These methods retain and compare untrusted observations only.  They never reconstruct live
//! progress, restore a permit or owner, or admit an operation to execution.

use rusqlite::types::ValueRef;
use rusqlite::{OptionalExtension, params};

use super::super::full_progress_codec::{
    FULL_PROGRESS_MAX_RECORD_BYTES, StoredFullProgressRecord, decode, encode,
};
use super::super::model::{MAX_RECORD_BYTES, OperationId};
#[cfg(test)]
use super::{InsertTestBoundary, InsertTestDirective};
use super::{OperationJournalStore, OperationJournalStoreError, audit_error};

/// One strictly validated full-progress row read from the profile-owned table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FullProgressRow {
    sequence: i64,
    record: StoredFullProgressRecord,
}

impl FullProgressRow {
    /// Return the positive global journal sequence retained for this observation.
    pub(crate) const fn sequence(&self) -> i64 {
        self.sequence
    }

    /// Borrow the validated, still-untrusted full-progress observation.
    pub(crate) fn record(&self) -> &StoredFullProgressRecord {
        &self.record
    }

    pub(crate) fn into_record(self) -> StoredFullProgressRecord {
        self.record
    }
}

/// Result of one full-progress expected-sequence write attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FullProgressWriteOutcome {
    /// A changed observation was committed at the newly allocated global sequence.
    Applied { sequence: i64 },
    /// The exact retained observation was replayed without allocating or writing.
    Replay { sequence: i64 },
}

impl FullProgressWriteOutcome {
    /// Return the current durable sequence acknowledged by this attempt.
    pub(crate) const fn sequence(self) -> i64 {
        match self {
            Self::Applied { sequence } | Self::Replay { sequence } => sequence,
        }
    }

    /// Return whether this attempt was an exact acknowledgement-loss replay.
    pub(crate) const fn is_replay(self) -> bool {
        matches!(self, Self::Replay { .. })
    }
}

impl OperationJournalStore {
    /// Read one validated full-progress row by operation id.
    ///
    /// A missing row in this family is ordinary absence, including when only the legacy family
    /// contains the operation.  A canonical id present in both families is retained as an audit
    /// condition so callers can never select one row and silently overwrite the other.
    pub(crate) fn get_full(
        &self,
        operation_id: OperationId,
    ) -> Result<Option<FullProgressRow>, OperationJournalStoreError> {
        self.validate_namespace()?;
        self.connection.execute_batch("BEGIN")?;
        let result = (|| {
            self.validate_namespace_for_precommit()?;
            let (legacy_present, full_progress_present) =
                self.operation_family_presence(operation_id)?;
            if legacy_present && full_progress_present {
                return Err(audit_error(
                    &self.database_path,
                    format!("operation id {operation_id} is present in both journal families"),
                ));
            }
            if full_progress_present {
                Ok(Some(self.read_full_row(operation_id)?.ok_or_else(
                    || {
                        full_progress_audit(
                            &self.database_path,
                            operation_id,
                            "full-progress row disappeared during a bounded read",
                        )
                    },
                )?))
            } else {
                Ok(None)
            }
        })();
        match result {
            Ok(result) => {
                if let Err(error) = self.validate_namespace_for_precommit() {
                    let _ = self.connection.execute_batch("ROLLBACK");
                    return Err(error);
                }
                if let Err(error) = self.connection.execute_batch("COMMIT") {
                    let _ = self.connection.execute_batch("ROLLBACK");
                    return Err(error.into());
                }
                self.validate_namespace()?;
                Ok(result)
            }
            Err(error) => {
                let _ = self.connection.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    /// Persist one validated full-progress observation using a global-sequence compare-and-swap.
    ///
    /// `None` is the initial insert expectation.  `Some(sequence)` is the caller's expected prior
    /// sequence.  Exact structured replays are read-only, including an acknowledgement-loss
    /// replay with an older expected sequence; changed observations may advance only from the
    /// current sequence and may not alter the frozen intent retained by the first observation.
    pub(crate) fn persist_full(
        &self,
        record: &StoredFullProgressRecord,
        expected_sequence: Option<i64>,
    ) -> Result<FullProgressWriteOutcome, OperationJournalStoreError> {
        record.validate().map_err(|error| {
            OperationJournalStoreError::InvalidFullProgressObservation {
                reason: error.to_string(),
            }
        })?;
        let encoded = encode(record).map_err(|error| {
            OperationJournalStoreError::InvalidFullProgressObservation {
                reason: error.to_string(),
            }
        })?;
        if encoded.len() > FULL_PROGRESS_MAX_RECORD_BYTES || encoded.len() > MAX_RECORD_BYTES {
            return Err(OperationJournalStoreError::InvalidFullProgressObservation {
                reason: format!(
                    "encoded full-progress record exceeds {FULL_PROGRESS_MAX_RECORD_BYTES} bytes"
                ),
            });
        }
        if let Some(expected) = expected_sequence.filter(|sequence| *sequence <= 0) {
            return Err(OperationJournalStoreError::FullProgressSequenceConflict {
                operation_id: record.snapshot().intent().operation_id(),
                expected: Some(expected),
                actual: None,
            });
        }

        self.validate_namespace()?;
        self.connection.execute_batch("BEGIN EXCLUSIVE")?;
        let result = self.persist_full_inside_transaction(record, &encoded, expected_sequence);
        match result {
            Ok(FullProgressWriteOutcome::Replay { sequence }) => {
                self.connection.execute_batch("ROLLBACK")?;
                self.validate_namespace()?;
                Ok(FullProgressWriteOutcome::Replay { sequence })
            }
            Ok(FullProgressWriteOutcome::Applied { sequence }) => {
                #[cfg(test)]
                if let Some(directive) =
                    self.insert_test_directive(InsertTestBoundary::BeforePrecommitValidation)
                {
                    match directive {
                        InsertTestDirective::Continue => {}
                        InsertTestDirective::Fail(error)
                        | InsertTestDirective::FailAndLeaveForRollback(error)
                        | InsertTestDirective::ExecuteThenFail(error) => {
                            let _ = self.connection.execute_batch("ROLLBACK");
                            return Err(error);
                        }
                    }
                }
                if let Err(error) = self.validate_namespace_for_precommit() {
                    let _ = self.connection.execute_batch("ROLLBACK");
                    return Err(error);
                }
                if let Err(error) = self.connection.execute_batch("COMMIT") {
                    let _ = self.connection.execute_batch("ROLLBACK");
                    return Err(error.into());
                }
                self.validate_namespace()?;
                let operation_id = record.snapshot().intent().operation_id();
                let persisted = self.get_full(operation_id)?;
                if persisted.as_ref().map(FullProgressRow::record) != Some(record)
                    || persisted.as_ref().map(FullProgressRow::sequence) != Some(sequence)
                {
                    return Err(audit_error(
                        &self.database_path,
                        format!(
                            "committed full-progress row for {operation_id} could not be read back identically"
                        ),
                    ));
                }
                Ok(FullProgressWriteOutcome::Applied { sequence })
            }
            Err(error) => {
                let _ = self.connection.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    fn persist_full_inside_transaction(
        &self,
        record: &StoredFullProgressRecord,
        encoded: &[u8],
        expected_sequence: Option<i64>,
    ) -> Result<FullProgressWriteOutcome, OperationJournalStoreError> {
        self.validate_namespace_for_precommit()?;
        let operation_id = record.snapshot().intent().operation_id();
        let key = operation_id.to_string();
        let (legacy_present, full_progress_present) =
            self.operation_family_presence(operation_id)?;
        if legacy_present && full_progress_present {
            return Err(audit_error(
                &self.database_path,
                format!("operation id {operation_id} is present in both journal families"),
            ));
        }
        if legacy_present {
            return Err(OperationJournalStoreError::DuplicateOperation { operation_id });
        }

        let current = if full_progress_present {
            Some(self.read_full_row(operation_id)?.ok_or_else(|| {
                full_progress_audit(
                    &self.database_path,
                    operation_id,
                    "full-progress row disappeared during the exclusive transaction",
                )
            })?)
        } else {
            None
        };
        let Some(current) = current else {
            if expected_sequence.is_some() {
                return Err(full_progress_conflict(
                    operation_id,
                    expected_sequence,
                    None,
                ));
            }
            let sequence = self.next_sequence_inside_transaction()?;
            self.connection.execute(
                "INSERT INTO operation_journal_full_progress (operation_id, sequence, record)
                 VALUES (?1, ?2, ?3)",
                params![key, sequence, encoded],
            )?;
            return Ok(FullProgressWriteOutcome::Applied { sequence });
        };

        if expected_sequence.is_some_and(|expected| expected > current.sequence) {
            return Err(full_progress_conflict(
                operation_id,
                expected_sequence,
                Some(current.sequence),
            ));
        }
        if current.record == *record {
            return Ok(FullProgressWriteOutcome::Replay {
                sequence: current.sequence,
            });
        }
        if expected_sequence != Some(current.sequence)
            || current.record.snapshot().intent() != record.snapshot().intent()
        {
            return Err(full_progress_conflict(
                operation_id,
                expected_sequence,
                Some(current.sequence),
            ));
        }

        let sequence = self.next_sequence_inside_transaction()?;
        let changed = self.connection.execute(
            "UPDATE operation_journal_full_progress
             SET sequence = ?1, record = ?2
             WHERE operation_id = ?3 AND sequence = ?4",
            params![sequence, encoded, key, current.sequence],
        )?;
        if changed != 1 {
            return Err(full_progress_conflict(
                operation_id,
                expected_sequence,
                Some(current.sequence),
            ));
        }
        Ok(FullProgressWriteOutcome::Applied { sequence })
    }

    fn read_full_row(
        &self,
        operation_id: OperationId,
    ) -> Result<Option<FullProgressRow>, OperationJournalStoreError> {
        let metadata = self
            .connection
            .query_row(
                "SELECT typeof(operation_id), length(CAST(operation_id AS BLOB)),
                        typeof(sequence), sequence, typeof(record), length(record),
                        CASE WHEN typeof(record) = 'blob'
                                  AND length(record) BETWEEN 0 AND ?2
                             THEN record ELSE NULL END
                 FROM operation_journal_full_progress WHERE operation_id = ?1",
                params![
                    operation_id.to_string(),
                    FULL_PROGRESS_MAX_RECORD_BYTES as i64
                ],
                |row| {
                    let operation_id_type: String = row.get(0)?;
                    let operation_id_length = match row.get_ref(1)? {
                        ValueRef::Integer(value) => Some(value),
                        ValueRef::Null => None,
                        _ => None,
                    };
                    let sequence_type: String = row.get(2)?;
                    let sequence = match row.get_ref(3)? {
                        ValueRef::Integer(value) => Some(value),
                        _ => None,
                    };
                    let record_type: String = row.get(4)?;
                    let record_length = match row.get_ref(5)? {
                        ValueRef::Integer(value) => Some(value),
                        ValueRef::Null => None,
                        _ => None,
                    };
                    let encoded = match row.get_ref(6)? {
                        ValueRef::Blob(value) => Some(value.to_vec()),
                        ValueRef::Null => None,
                        _ => None,
                    };
                    Ok((
                        operation_id_type,
                        operation_id_length,
                        sequence_type,
                        sequence,
                        record_type,
                        record_length,
                        encoded,
                    ))
                },
            )
            .optional()?;
        let Some((
            operation_id_type,
            operation_id_length,
            sequence_type,
            sequence,
            record_type,
            record_length,
            encoded,
        )) = metadata
        else {
            return Ok(None);
        };
        if operation_id_type != "text" {
            return Err(full_progress_audit(
                &self.database_path,
                operation_id,
                "operation id column must contain a SQLite TEXT key",
            ));
        }
        if operation_id_length != Some(operation_id.to_string().len() as i64) {
            return Err(full_progress_audit(
                &self.database_path,
                operation_id,
                "operation id column length does not match its canonical key",
            ));
        }
        if sequence_type != "integer" || sequence.is_none() {
            return Err(full_progress_audit(
                &self.database_path,
                operation_id,
                "sequence column must contain a SQLite integer",
            ));
        }
        let sequence = sequence.expect("checked above");
        if sequence <= 0 {
            return Err(full_progress_audit(
                &self.database_path,
                operation_id,
                "stored sequence is not positive",
            ));
        }
        self.ensure_sequence_unique(sequence)?;
        if record_type != "blob" {
            return Err(full_progress_audit(
                &self.database_path,
                operation_id,
                "record column must contain a SQLite BLOB",
            ));
        }
        let Some(record_length) = record_length else {
            return Err(full_progress_audit(
                &self.database_path,
                operation_id,
                "stored record length is NULL",
            ));
        };
        if !(0..=FULL_PROGRESS_MAX_RECORD_BYTES as i64).contains(&record_length) {
            return Err(full_progress_audit(
                &self.database_path,
                operation_id,
                format!(
                    "stored record length {record_length} exceeds the bounded full-progress record size"
                ),
            ));
        }

        let Some(encoded) = encoded else {
            return Err(full_progress_audit(
                &self.database_path,
                operation_id,
                "stored record payload was not a bounded SQLite BLOB",
            ));
        };
        if encoded.len() != record_length as usize || encoded.len() > FULL_PROGRESS_MAX_RECORD_BYTES
        {
            return Err(full_progress_audit(
                &self.database_path,
                operation_id,
                "stored record length changed or exceeded the full-progress bound",
            ));
        }
        let record = decode(&encoded).map_err(|error| {
            full_progress_audit(
                &self.database_path,
                operation_id,
                format!("stored full-progress record failed bounded decode: {error}"),
            )
        })?;
        if record.snapshot().intent().operation_id() != operation_id {
            return Err(full_progress_audit(
                &self.database_path,
                operation_id,
                "record identity does not match its SQLite key",
            ));
        }
        Ok(Some(FullProgressRow { sequence, record }))
    }
}

fn full_progress_conflict(
    operation_id: OperationId,
    expected: Option<i64>,
    actual: Option<i64>,
) -> OperationJournalStoreError {
    OperationJournalStoreError::FullProgressSequenceConflict {
        operation_id,
        expected,
        actual,
    }
}

fn full_progress_audit(
    path: &std::path::Path,
    operation_id: OperationId,
    reason: impl Into<String>,
) -> OperationJournalStoreError {
    audit_error(
        path,
        format!("full-progress operation {operation_id}: {}", reason.into()),
    )
}
