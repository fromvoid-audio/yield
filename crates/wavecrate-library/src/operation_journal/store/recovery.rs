//! Bounded, read-only paging for profile-owned operation-journal recovery.
//!
//! A recovery page is an observation of rows that are already in the journal. It never updates
//! the database, opens a filesystem path from a record, or turns stored evidence into execution
//! authority. The scan uses the insertion sequence as a keyset cursor and keeps both row count
//! and decoded record bytes bounded.

use rusqlite::types::ValueRef;
use rusqlite::{OptionalExtension, params};
use thiserror::Error;

use super::{JOURNAL_TABLE_NAME, OperationJournalStore, OperationJournalStoreError};
use crate::operation_journal::codec::{DecodeError, StoredOperationRecord, decode};
use crate::operation_journal::model::{MAX_RECORD_BYTES, OperationId};
use crate::operation_journal::validation::RecordConsistency;

/// Maximum number of rows returned by one recovery page.
pub const MAX_RECOVERY_ROWS_PER_PAGE: usize = 16;
/// Maximum encoded record bytes loaded by one recovery page.
pub const MAX_RECOVERY_BYTES_PER_PAGE: usize = 2 * 1024 * 1024;

const PAGE_QUERY_LIMIT: i64 = (MAX_RECOVERY_ROWS_PER_PAGE + 1) as i64;
const MAX_OPERATION_KEY_BYTES: i64 = 128;
const MAX_OPERATION_KEY_PREVIEW_BYTES: i64 = MAX_OPERATION_KEY_BYTES + 1;
const MAX_DIAGNOSTIC_CHARS: usize = 256;

/// An opaque positive insertion-sequence cursor returned by a recovery page.
///
/// A cursor only selects a later observation page. It is not a mutation token, admission proof,
/// or filesystem capability.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RecoveryCursor(i64);

impl RecoveryCursor {
    /// Create a cursor from a positive stored insertion sequence.
    pub fn new(sequence: i64) -> Option<Self> {
        (sequence > 0).then_some(Self(sequence))
    }

    /// Return the stored sequence represented by this cursor.
    pub fn sequence(self) -> i64 {
        self.0
    }
}

/// Bounded SQL key information attached to one recovered row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryKey {
    /// The SQL key was a valid operation UUID.
    OperationId(OperationId),
    /// The SQL key was text but did not parse as an operation UUID.
    Text(String),
    /// The SQL key could not be represented as a bounded text key.
    Invalid {
        /// SQLite's reported type for the key.
        sql_type: String,
        /// Bounded diagnostic or preview of the rejected key.
        preview: String,
    },
}

/// The stored field whose SQL representation was invalid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryField {
    /// The operation-id key column.
    OperationId,
    /// The insertion-sequence column.
    Sequence,
    /// The encoded-record column.
    Record,
}

/// A typed reason why a journal row remains retained for audit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryRetentionReason {
    /// A legacy schema-v1 post-publication row has no publication evidence.
    LegacyMissingPublicationEvidence,
    /// The record uses a codec schema that this build does not understand.
    UnsupportedCodec {
        /// Unsupported persisted schema version.
        version: u32,
    },
    /// The record bytes or pure consistency relationships are malformed.
    Malformed {
        /// Bounded diagnostic for the malformed record.
        reason: String,
    },
    /// The decoded record identity differs from the SQL key.
    KeyMismatch {
        /// Operation identity decoded from the record.
        record_operation_id: OperationId,
    },
    /// The SQL length or encoded byte size cannot be accepted under the record bound.
    SizeInvalid {
        /// Length reported by SQLite, when available.
        declared: Option<i64>,
        /// Bounded diagnostic for the rejected size.
        reason: String,
    },
    /// A SQL column or value had an unexpected type or bounded representation.
    TypeInvalid {
        /// Column whose representation was rejected.
        field: RecoveryField,
        /// SQLite's reported type for that column.
        sql_type: String,
        /// Bounded diagnostic for the rejected value.
        reason: String,
    },
}

/// Outcome for one row returned by a recovery page.
#[derive(Clone, Debug, PartialEq)]
pub enum RecoveryRowOutcome {
    /// The decoded record passed pure consistency classification.
    Consistent(StoredOperationRecord),
    /// The row remains retained data and requires a later audit decision.
    Retained(RecoveryRetentionReason),
}

/// One bounded journal row observation.
#[derive(Clone, Debug, PartialEq)]
pub struct RecoveryEntry {
    key: RecoveryKey,
    sequence: i64,
    outcome: RecoveryRowOutcome,
}

impl RecoveryEntry {
    /// Return the bounded SQL key observation.
    pub fn key(&self) -> &RecoveryKey {
        &self.key
    }

    /// Return the positive insertion sequence for this row.
    pub fn sequence(&self) -> i64 {
        self.sequence
    }

    /// Return the decoded record or retained audit reason.
    pub fn outcome(&self) -> &RecoveryRowOutcome {
        &self.outcome
    }
}

/// One bounded read-only recovery page.
#[derive(Clone, Debug, PartialEq)]
pub struct RecoveryPage {
    entries: Vec<RecoveryEntry>,
    next_cursor: Option<RecoveryCursor>,
    done: bool,
    encoded_bytes: usize,
}

impl RecoveryPage {
    /// Return row observations in ascending insertion-sequence order.
    pub fn entries(&self) -> &[RecoveryEntry] {
        &self.entries
    }

    /// Consume this page and return its row observations.
    pub fn into_entries(self) -> Vec<RecoveryEntry> {
        self.entries
    }

    /// Return the cursor to pass for another page, or `None` when this page is complete.
    pub fn next_cursor(&self) -> Option<RecoveryCursor> {
        self.next_cursor
    }

    /// Return whether the page reached the end of the journal.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Return the encoded record bytes loaded while constructing this page.
    pub fn encoded_bytes(&self) -> usize {
        self.encoded_bytes
    }
}

/// Failure while reading a bounded recovery page.
#[derive(Debug, Error)]
pub enum RecoveryScanError {
    /// A profile-owned store or namespace check failed.
    #[error(transparent)]
    Store(#[from] OperationJournalStoreError),
    /// The journal contains a sequence that cannot provide a stable recovery cursor.
    #[error("operation journal recovery scan requires audit: {reason}")]
    RequiresAudit {
        /// Bounded diagnostic for the scan-level refusal.
        reason: String,
    },
}

pub(super) fn scan_page(
    store: &OperationJournalStore,
    cursor: Option<RecoveryCursor>,
) -> Result<RecoveryPage, RecoveryScanError> {
    store
        .validate_namespace()
        .map_err(RecoveryScanError::Store)?;
    let result = scan_page_inner(store, cursor);
    let post_validation = store.validate_namespace().map_err(RecoveryScanError::Store);
    match post_validation {
        Err(error) => Err(error),
        Ok(()) => result,
    }
}

fn scan_page_inner(
    store: &OperationJournalStore,
    cursor: Option<RecoveryCursor>,
) -> Result<RecoveryPage, RecoveryScanError> {
    let statement = match cursor {
        Some(_) => format!(
            "SELECT rowid,
                    typeof(operation_id), length(CAST(operation_id AS BLOB)),
                    CASE WHEN typeof(operation_id) = 'text'
                         THEN substr(CAST(operation_id AS BLOB), 1, {MAX_OPERATION_KEY_PREVIEW_BYTES})
                         ELSE NULL END,
                    sequence, typeof(sequence),
                    typeof(record), length(record)
             FROM {JOURNAL_TABLE_NAME}
             WHERE sequence > ?1
             ORDER BY sequence ASC, rowid ASC
             LIMIT ?2"
        ),
        None => format!(
            "SELECT rowid,
                    typeof(operation_id), length(CAST(operation_id AS BLOB)),
                    CASE WHEN typeof(operation_id) = 'text'
                         THEN substr(CAST(operation_id AS BLOB), 1, {MAX_OPERATION_KEY_PREVIEW_BYTES})
                         ELSE NULL END,
                    sequence, typeof(sequence),
                    typeof(record), length(record)
             FROM {JOURNAL_TABLE_NAME}
             ORDER BY sequence ASC, rowid ASC
             LIMIT ?1"
        ),
    };
    let mut rows = store.connection.prepare(&statement).map_err(store_error)?;
    let mut candidates = Vec::with_capacity(MAX_RECOVERY_ROWS_PER_PAGE + 1);
    let mut query_rows = match cursor {
        Some(cursor) => rows
            .query(params![cursor.sequence(), PAGE_QUERY_LIMIT])
            .map_err(store_error)?,
        None => rows.query(params![PAGE_QUERY_LIMIT]).map_err(store_error)?,
    };
    while let Some(row) = query_rows.next().map_err(store_error)? {
        candidates.push(read_candidate(row)?);
    }
    drop(query_rows);
    drop(rows);

    validate_candidate_sequences(&candidates, cursor)?;
    if candidates.is_empty() {
        return Ok(RecoveryPage {
            entries: Vec::new(),
            next_cursor: None,
            done: true,
            encoded_bytes: 0,
        });
    }

    let mut entries = Vec::with_capacity(MAX_RECOVERY_ROWS_PER_PAGE);
    let mut encoded_bytes = 0_usize;
    let mut deferred = false;
    let mut last_sequence = cursor.map(RecoveryCursor::sequence);

    for candidate in &candidates {
        if entries.len() >= MAX_RECOVERY_ROWS_PER_PAGE {
            deferred = true;
            break;
        }

        if let Some(reason) = candidate.key_issue.as_ref() {
            entries.push(RecoveryEntry {
                key: candidate.key.clone(),
                sequence: candidate.sequence,
                outcome: RecoveryRowOutcome::Retained(reason.clone()),
            });
            last_sequence = Some(candidate.sequence);
            continue;
        }

        if candidate.record_type != "blob" {
            entries.push(RecoveryEntry {
                key: candidate.key.clone(),
                sequence: candidate.sequence,
                outcome: RecoveryRowOutcome::Retained(RecoveryRetentionReason::TypeInvalid {
                    field: RecoveryField::Record,
                    sql_type: candidate.record_type.clone(),
                    reason: String::from("record column must contain a SQLite BLOB"),
                }),
            });
            last_sequence = Some(candidate.sequence);
            continue;
        }

        let Some(declared_length) = candidate.record_length else {
            entries.push(size_invalid_entry(candidate, None, "record length is NULL"));
            last_sequence = Some(candidate.sequence);
            continue;
        };
        if declared_length < 0 || declared_length as usize > MAX_RECORD_BYTES {
            entries.push(size_invalid_entry(
                candidate,
                Some(declared_length),
                "record length exceeds the bounded record size",
            ));
            last_sequence = Some(candidate.sequence);
            continue;
        }
        let declared_length = declared_length as usize;
        if encoded_bytes.saturating_add(declared_length) > MAX_RECOVERY_BYTES_PER_PAGE {
            deferred = true;
            break;
        }

        let loaded = load_record_bytes(store, candidate)?;
        let Some(encoded) = loaded else {
            entries.push(size_invalid_entry(
                candidate,
                Some(declared_length as i64),
                "record disappeared before its bounded value could be read",
            ));
            last_sequence = Some(candidate.sequence);
            continue;
        };
        if encoded.len() != declared_length {
            // Do not continue with an inaccurate page byte budget after an unexpected change.
            return Err(RecoveryScanError::RequiresAudit {
                reason: String::from("record length changed during bounded recovery read"),
            });
        }
        encoded_bytes = encoded_bytes.saturating_add(encoded.len());
        let outcome = decode_outcome(candidate, &encoded);
        entries.push(RecoveryEntry {
            key: candidate.key.clone(),
            sequence: candidate.sequence,
            outcome,
        });
        last_sequence = Some(candidate.sequence);
    }

    let Some(last_sequence) = last_sequence else {
        return Err(RecoveryScanError::RequiresAudit {
            reason: String::from("the first recovery row could not advance a positive cursor"),
        });
    };
    if entries.is_empty() && deferred {
        return Err(RecoveryScanError::RequiresAudit {
            reason: String::from("the first recovery row cannot fit within the page bound"),
        });
    }
    ensure_sequence_unique(store, last_sequence)?;

    let done = !deferred && candidates.len() <= MAX_RECOVERY_ROWS_PER_PAGE;
    Ok(RecoveryPage {
        entries,
        next_cursor: (!done).then_some(RecoveryCursor(last_sequence)),
        done,
        encoded_bytes,
    })
}

#[derive(Clone, Debug)]
struct RecoveryCandidate {
    rowid: i64,
    key: RecoveryKey,
    key_issue: Option<RecoveryRetentionReason>,
    sequence: i64,
    record_type: String,
    record_length: Option<i64>,
}

fn read_candidate(row: &rusqlite::Row<'_>) -> Result<RecoveryCandidate, RecoveryScanError> {
    let rowid = row.get(0).map_err(store_error)?;
    let key_type: String = row.get(1).map_err(store_error)?;
    let key_length: Option<i64> = row.get(2).map_err(store_error)?;
    let key_preview = match row.get_ref(3).map_err(store_error)? {
        ValueRef::Null => None,
        ValueRef::Blob(bytes) => Some(bytes.to_vec()),
        _ => {
            return Err(RecoveryScanError::RequiresAudit {
                reason: String::from("SQLite key preview returned an unexpected type"),
            });
        }
    };
    let sequence_type: String = row.get(5).map_err(store_error)?;
    if sequence_type != "integer" {
        return Err(RecoveryScanError::RequiresAudit {
            reason: bounded_reason(format!(
                "journal row {rowid} has sequence SQL type {sequence_type}"
            )),
        });
    }
    let sequence: i64 = row.get(4).map_err(store_error)?;
    if sequence <= 0 {
        return Err(RecoveryScanError::RequiresAudit {
            reason: bounded_reason(format!(
                "journal row {rowid} has non-positive insertion sequence {sequence}"
            )),
        });
    }
    let (key, key_issue) = read_key(key_type, key_length, key_preview);
    let record_type: String = row.get(6).map_err(store_error)?;
    let record_length: Option<i64> = row.get(7).map_err(store_error)?;
    Ok(RecoveryCandidate {
        rowid,
        key,
        key_issue,
        sequence,
        record_type,
        record_length,
    })
}

fn read_key(
    key_type: String,
    key_length: Option<i64>,
    key_preview: Option<Vec<u8>>,
) -> (RecoveryKey, Option<RecoveryRetentionReason>) {
    if key_type != "text" {
        let reason = format!("operation_id column must contain SQLite TEXT, found {key_type}");
        return (
            RecoveryKey::Invalid {
                sql_type: key_type.clone(),
                preview: bounded_reason(&reason),
            },
            Some(RecoveryRetentionReason::TypeInvalid {
                field: RecoveryField::OperationId,
                sql_type: key_type,
                reason: bounded_reason(reason),
            }),
        );
    }
    let Some(key_length) = key_length else {
        let reason = String::from("operation_id text length is NULL");
        return (
            RecoveryKey::Invalid {
                sql_type: String::from("text"),
                preview: reason.clone(),
            },
            Some(RecoveryRetentionReason::TypeInvalid {
                field: RecoveryField::OperationId,
                sql_type: String::from("text"),
                reason,
            }),
        );
    };
    if !(0..=MAX_OPERATION_KEY_BYTES).contains(&key_length) {
        let reason = format!("operation_id byte length {key_length} exceeds the bounded key size");
        return (
            RecoveryKey::Invalid {
                sql_type: String::from("text"),
                preview: bounded_reason(reason.clone()),
            },
            Some(RecoveryRetentionReason::SizeInvalid {
                declared: Some(key_length),
                reason: bounded_reason(reason),
            }),
        );
    }
    let Some(key_preview) = key_preview else {
        let reason = String::from("operation_id text preview is unavailable");
        return (
            RecoveryKey::Invalid {
                sql_type: String::from("text"),
                preview: reason.clone(),
            },
            Some(RecoveryRetentionReason::TypeInvalid {
                field: RecoveryField::OperationId,
                sql_type: String::from("text"),
                reason,
            }),
        );
    };
    let Ok(key_text) = String::from_utf8(key_preview) else {
        let reason = String::from("operation_id text is not valid UTF-8");
        return (
            RecoveryKey::Invalid {
                sql_type: String::from("text"),
                preview: reason.clone(),
            },
            Some(RecoveryRetentionReason::TypeInvalid {
                field: RecoveryField::OperationId,
                sql_type: String::from("text"),
                reason,
            }),
        );
    };
    let key_text = bounded_string(key_text);
    match OperationId::parse(&key_text) {
        Ok(operation_id) if operation_id.to_string() == key_text => {
            (RecoveryKey::OperationId(operation_id), None)
        }
        Err(_) => (
            RecoveryKey::Text(key_text),
            Some(RecoveryRetentionReason::TypeInvalid {
                field: RecoveryField::OperationId,
                sql_type: String::from("text"),
                reason: String::from("operation_id text is not the canonical UUID spelling"),
            }),
        ),
        Ok(_) => (
            RecoveryKey::Text(key_text),
            Some(RecoveryRetentionReason::TypeInvalid {
                field: RecoveryField::OperationId,
                sql_type: String::from("text"),
                reason: String::from("operation_id text is not the canonical UUID spelling"),
            }),
        ),
    }
}

fn validate_candidate_sequences(
    candidates: &[RecoveryCandidate],
    cursor: Option<RecoveryCursor>,
) -> Result<(), RecoveryScanError> {
    let mut previous = cursor.map(RecoveryCursor::sequence);
    for candidate in candidates {
        if let Some(cursor) = previous {
            if candidate.sequence <= cursor {
                return Err(RecoveryScanError::RequiresAudit {
                    reason: bounded_reason(format!(
                        "recovery sequence {} did not advance past cursor {cursor}",
                        candidate.sequence
                    )),
                });
            }
        }
        previous = Some(candidate.sequence);
    }
    Ok(())
}

fn load_record_bytes(
    store: &OperationJournalStore,
    candidate: &RecoveryCandidate,
) -> Result<Option<Vec<u8>>, RecoveryScanError> {
    let metadata: Option<(String, Option<i64>)> = store
        .connection
        .query_row(
            "SELECT typeof(record), length(record) FROM operation_journal WHERE rowid = ?1",
            params![candidate.rowid],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(store_error)?;
    let Some((record_type, record_length)) = metadata else {
        return Ok(None);
    };
    if record_type != "blob" || record_length != candidate.record_length {
        return Err(RecoveryScanError::RequiresAudit {
            reason: bounded_reason(format!(
                "journal row {} changed while recovery metadata was being read",
                candidate.rowid
            )),
        });
    }
    let Some(record_length) = record_length else {
        return Ok(None);
    };
    if !(0..=MAX_RECORD_BYTES as i64).contains(&record_length) {
        return Ok(None);
    }
    let encoded: Vec<u8> = store
        .connection
        .query_row(
            "SELECT record FROM operation_journal WHERE rowid = ?1",
            params![candidate.rowid],
            |row| row.get(0),
        )
        .map_err(store_error)?;
    Ok(Some(encoded))
}

fn decode_outcome(candidate: &RecoveryCandidate, encoded: &[u8]) -> RecoveryRowOutcome {
    let record = match decode(encoded) {
        Ok(record) => record,
        Err(DecodeError::UnsupportedVersion { version }) => {
            return RecoveryRowOutcome::Retained(RecoveryRetentionReason::UnsupportedCodec {
                version,
            });
        }
        Err(DecodeError::Oversize { actual, maximum }) => {
            return RecoveryRowOutcome::Retained(RecoveryRetentionReason::SizeInvalid {
                declared: Some(actual as i64),
                reason: bounded_reason(format!(
                    "decoded record size {actual} exceeds maximum {maximum}"
                )),
            });
        }
        Err(DecodeError::Malformed { reason }) => {
            return RecoveryRowOutcome::Retained(RecoveryRetentionReason::Malformed {
                reason: bounded_reason(reason),
            });
        }
    };
    if let RecoveryKey::OperationId(operation_id) = candidate.key {
        if record.operation_id != operation_id {
            return RecoveryRowOutcome::Retained(RecoveryRetentionReason::KeyMismatch {
                record_operation_id: record.operation_id,
            });
        }
    }
    match super::super::validation::classify_record(&record) {
        RecordConsistency::Consistent => RecoveryRowOutcome::Consistent(record),
        RecordConsistency::LegacyMissingPublicationEvidence => {
            RecoveryRowOutcome::Retained(RecoveryRetentionReason::LegacyMissingPublicationEvidence)
        }
        RecordConsistency::Invalid(reason) => {
            RecoveryRowOutcome::Retained(RecoveryRetentionReason::Malformed {
                reason: bounded_reason(reason),
            })
        }
    }
}

fn size_invalid_entry(
    candidate: &RecoveryCandidate,
    declared: Option<i64>,
    reason: impl Into<String>,
) -> RecoveryEntry {
    let reason = reason.into();
    RecoveryEntry {
        key: candidate.key.clone(),
        sequence: candidate.sequence,
        outcome: RecoveryRowOutcome::Retained(RecoveryRetentionReason::SizeInvalid {
            declared,
            reason: bounded_reason(reason),
        }),
    }
}

fn ensure_sequence_unique(
    store: &OperationJournalStore,
    sequence: i64,
) -> Result<(), RecoveryScanError> {
    let mut statement = store
        .connection
        .prepare(
            "SELECT rowid FROM operation_journal
             WHERE typeof(sequence) = 'integer' AND sequence = ?1
             LIMIT 2",
        )
        .map_err(store_error)?;
    let mut rows = statement.query(params![sequence]).map_err(store_error)?;
    let mut count = 0_usize;
    while rows.next().map_err(store_error)?.is_some() {
        count += 1;
        if count >= 2 {
            return Err(RecoveryScanError::RequiresAudit {
                reason: bounded_reason(format!("duplicate recovery insertion sequence {sequence}")),
            });
        }
    }
    Ok(())
}

fn store_error(error: rusqlite::Error) -> RecoveryScanError {
    if let rusqlite::Error::SqliteFailure(sqlite_error, _) = &error {
        if sqlite_error.code == rusqlite::ffi::ErrorCode::TooBig {
            return RecoveryScanError::RequiresAudit {
                reason: String::from(
                    "SQLite refused a journal value above the configured recovery bound",
                ),
            };
        }
    }
    RecoveryScanError::Store(OperationJournalStoreError::Sql(error))
}

fn bounded_reason(reason: impl std::fmt::Display) -> String {
    reason
        .to_string()
        .chars()
        .take(MAX_DIAGNOSTIC_CHARS)
        .collect()
}

fn bounded_string(value: String) -> String {
    value
        .chars()
        .take(MAX_OPERATION_KEY_BYTES as usize)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use tempfile::TempDir;
    use uuid::Uuid;

    use super::*;
    use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard, WritableProfileGuard};
    use crate::operation_journal::codec::{decode, encode};
    use crate::test_runtime::TestRuntimeGuard;

    struct Fixture {
        owner: WritableProfileGuard,
        _profile: PersistenceProfileGuard,
        _config: ConfigBaseGuard,
        _base: TempDir,
        _runtime: TestRuntimeGuard,
    }

    fn fixture() -> Fixture {
        let runtime = TestRuntimeGuard::acquire();
        let base = tempfile::tempdir().expect("temporary config base");
        let base_path = fs::canonicalize(base.path()).expect("canonical temporary config base");
        let config = ConfigBaseGuard::set(base_path);
        let profile =
            PersistenceProfileGuard::named(format!("journal-recovery-{}", Uuid::new_v4().simple()));
        let owner = WritableProfileGuard::acquire_current().expect("profile owner");
        Fixture {
            owner,
            _profile: profile,
            _config: config,
            _base: base,
            _runtime: runtime,
        }
    }

    fn intent_record(operation_id: OperationId) -> StoredOperationRecord {
        let value = serde_json::json!({
            "schema_version": 1,
            "operation_id": operation_id.as_uuid(),
            "intent": {
                "actor": "User",
                "kind": "FileMutation",
                "label": "recovery-test"
            },
            "phase": "IntentDurable",
            "disposition": "None",
            "payload": {},
            "capacity_plan": {
                "volumes": [{
                    "identity": {"device": 1},
                    "allocation_unit": 1,
                    "allocation_class": "DestinationStaging",
                    "logical_bytes": 1,
                    "allocated_bytes": 1,
                    "protected_free_bytes": 268435456
                }]
            },
            "prepared": null,
            "staged": null,
            "published": null,
            "replacement_qualification": null,
            "created_unix_ms": 1,
            "updated_unix_ms": 1
        });
        decode(&serde_json::to_vec(&value).expect("serialize intent fixture"))
            .expect("decode intent fixture")
    }

    fn insert_raw(store: &OperationJournalStore, key: &str, sequence: i64, record: &[u8]) {
        store
            .connection
            .execute(
                "INSERT INTO operation_journal (operation_id, sequence, record)
                 VALUES (?1, ?2, ?3)",
                params![key, sequence, record],
            )
            .expect("insert recovery fixture row");
    }

    fn recovery_key(entry: &RecoveryEntry) -> OperationId {
        match entry.key() {
            RecoveryKey::OperationId(operation_id) => *operation_id,
            other => panic!("expected operation key, found {other:?}"),
        }
    }

    #[test]
    fn empty_page_is_done_without_a_cursor() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let page = store.scan_recovery_page(None).expect("scan empty journal");
        assert!(page.entries().is_empty());
        assert!(page.is_done());
        assert_eq!(page.next_cursor(), None);
        assert_eq!(page.encoded_bytes(), 0);
    }

    #[test]
    fn pages_have_sixteen_rows_and_progress_by_sequence() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let mut operation_ids = Vec::new();
        for _ in 0..17 {
            let record = intent_record(OperationId::new());
            operation_ids.push(record.operation_id);
            store.insert_intent(&record).expect("insert page fixture");
        }

        let first = store.scan_recovery_page(None).expect("scan first page");
        assert_eq!(first.entries().len(), MAX_RECOVERY_ROWS_PER_PAGE);
        assert!(!first.is_done());
        assert_eq!(first.next_cursor().map(RecoveryCursor::sequence), Some(16));
        assert_eq!(
            first
                .entries()
                .iter()
                .map(RecoveryEntry::sequence)
                .collect::<Vec<_>>(),
            (1..=16).collect::<Vec<_>>()
        );

        let second = store
            .scan_recovery_page(first.next_cursor())
            .expect("scan second page");
        assert_eq!(second.entries().len(), 1);
        assert!(second.is_done());
        assert_eq!(second.next_cursor(), None);
        assert_eq!(second.entries()[0].sequence(), 17);
        assert_eq!(recovery_key(&second.entries()[0]), operation_ids[16]);
    }

    #[test]
    fn page_defers_a_row_that_exceeds_remaining_byte_budget() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let small = intent_record(OperationId::new());
        let small_bytes = encode(&small).expect("encode small fixture").len();
        let mut empty_payload = intent_record(OperationId::new());
        empty_payload.payload = serde_json::Value::String(String::new());
        let envelope_bytes = encode(&empty_payload)
            .expect("encode empty payload fixture")
            .len();
        let payload_size = MAX_RECORD_BYTES - envelope_bytes;
        let mut first = intent_record(OperationId::new());
        first.payload = serde_json::Value::String("x".repeat(payload_size));
        let first_bytes = encode(&first).expect("encode first large fixture").len();
        let mut second = intent_record(OperationId::new());
        second.payload = serde_json::Value::String("y".repeat(payload_size));
        let second_bytes = encode(&second).expect("encode second large fixture").len();
        assert_eq!(first_bytes, MAX_RECORD_BYTES);
        assert_eq!(second_bytes, MAX_RECORD_BYTES);
        assert!(first_bytes + second_bytes + small_bytes > MAX_RECOVERY_BYTES_PER_PAGE);
        store
            .insert_intent(&first)
            .expect("insert first large fixture");
        store
            .insert_intent(&second)
            .expect("insert second large fixture");
        store
            .insert_intent(&small)
            .expect("insert deferred fixture");

        let first_page = store
            .scan_recovery_page(None)
            .expect("scan byte-budget page");
        assert_eq!(first_page.entries().len(), 2);
        assert!(!first_page.is_done());
        assert_eq!(
            first_page.encoded_bytes(),
            first_bytes + second_bytes,
            "only loaded record bytes count against the page budget"
        );
        let second_page = store
            .scan_recovery_page(first_page.next_cursor())
            .expect("scan deferred row");
        assert!(second_page.is_done());
        assert_eq!(second_page.entries().len(), 1);
        assert_eq!(second_page.entries()[0].sequence(), 3);
        assert_eq!(second_page.encoded_bytes(), small_bytes);
    }

    #[test]
    fn retained_rows_are_typed_and_cursor_continues_without_loading_oversize_blob() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let malformed_id = OperationId::new();
        insert_raw(&store, &malformed_id.to_string(), 1, b"not-json");
        let future_id = OperationId::new();
        insert_raw(
            &store,
            &future_id.to_string(),
            2,
            br#"{"schema_version":999}"#,
        );
        let oversized_id = OperationId::new();
        store
            .connection
            .pragma_update(None, "ignore_check_constraints", true)
            .expect("disable check for corrupt fixture");
        store
            .connection
            .execute(
                "INSERT INTO operation_journal (operation_id, sequence, record)
                 VALUES (?1, 3, zeroblob(?2))",
                params![oversized_id.to_string(), (MAX_RECORD_BYTES + 1) as i64],
            )
            .expect("insert oversized fixture");
        store
            .connection
            .pragma_update(None, "ignore_check_constraints", false)
            .expect("restore check constraints");
        let text_record_id = OperationId::new();
        store
            .connection
            .execute(
                "INSERT INTO operation_journal (operation_id, sequence, record)
                 VALUES (?1, 4, 'not-a-blob')",
                params![text_record_id.to_string()],
            )
            .expect("insert text record fixture");

        let page = store.scan_recovery_page(None).expect("scan retained rows");
        assert!(page.is_done());
        assert_eq!(page.entries().len(), 4);
        assert!(matches!(
            page.entries()[0].outcome(),
            RecoveryRowOutcome::Retained(RecoveryRetentionReason::Malformed { .. })
        ));
        assert!(matches!(
            page.entries()[1].outcome(),
            RecoveryRowOutcome::Retained(RecoveryRetentionReason::UnsupportedCodec {
                version: 999
            })
        ));
        assert!(matches!(
            page.entries()[2].outcome(),
            RecoveryRowOutcome::Retained(RecoveryRetentionReason::SizeInvalid { .. })
        ));
        assert!(matches!(
            page.entries()[3].outcome(),
            RecoveryRowOutcome::Retained(RecoveryRetentionReason::TypeInvalid {
                field: RecoveryField::Record,
                sql_type,
                ..
            }) if sql_type == "text"
        ));
        assert_eq!(page.next_cursor(), None);
        assert!(page.encoded_bytes() < 100);
    }

    #[test]
    fn key_mismatch_and_invalid_key_are_retained_with_bounded_keys() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let record_id = OperationId::new();
        let key_id = OperationId::new();
        let record = intent_record(record_id);
        let encoded = encode(&record).expect("encode mismatch fixture");
        insert_raw(&store, &key_id.to_string(), 1, &encoded);
        let valid_for_invalid_key = intent_record(OperationId::new());
        let encoded = encode(&valid_for_invalid_key).expect("encode invalid-key fixture");
        insert_raw(&store, "not-an-operation-id", 2, &encoded);
        let embedded_nul_key = format!("{}\0suffix", OperationId::new());
        insert_raw(&store, &embedded_nul_key, 3, &encoded);
        let uppercase_key = OperationId::new().to_string().to_uppercase();
        insert_raw(&store, &uppercase_key, 4, &encoded);

        let page = store.scan_recovery_page(None).expect("scan key fixtures");
        assert_eq!(page.entries().len(), 4);
        assert!(matches!(
            page.entries()[0].outcome(),
            RecoveryRowOutcome::Retained(RecoveryRetentionReason::KeyMismatch {
                record_operation_id
            }) if *record_operation_id == record_id
        ));
        assert!(matches!(
            page.entries()[1].outcome(),
            RecoveryRowOutcome::Retained(RecoveryRetentionReason::TypeInvalid {
                field: RecoveryField::OperationId,
                ..
            })
        ));
        assert!(
            matches!(page.entries()[1].key(), RecoveryKey::Text(value) if value == "not-an-operation-id")
        );
        assert!(matches!(
            page.entries()[2].outcome(),
            RecoveryRowOutcome::Retained(RecoveryRetentionReason::TypeInvalid {
                field: RecoveryField::OperationId,
                ..
            })
        ));
        assert!(matches!(
            page.entries()[2].key(),
            RecoveryKey::Text(value) if value.ends_with("\0suffix")
        ));
        assert!(matches!(
            page.entries()[3].outcome(),
            RecoveryRowOutcome::Retained(RecoveryRetentionReason::TypeInvalid {
                field: RecoveryField::OperationId,
                ..
            })
        ));
        assert!(matches!(
            page.entries()[3].key(),
            RecoveryKey::Text(value) if value.as_str() == value.to_uppercase()
        ));
    }

    #[test]
    fn invalid_sequence_stops_scan_before_it_can_hide_behind_a_cursor() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let record = intent_record(OperationId::new());
        let encoded = encode(&record).expect("encode invalid-sequence fixture");
        store
            .connection
            .execute(
                "INSERT INTO operation_journal (operation_id, sequence, record)
                 VALUES (?1, 'not-a-sequence', ?2)",
                params![record.operation_id.to_string(), encoded],
            )
            .expect("insert invalid sequence fixture");
        let error = store
            .scan_recovery_page(None)
            .expect_err("reject invalid sequence as scan-level audit");
        assert!(
            matches!(error, RecoveryScanError::RequiresAudit { reason } if reason.contains("sequence"))
        );
    }

    #[test]
    fn nonpositive_sequence_stops_scan_with_audit_reason() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let record = intent_record(OperationId::new());
        let encoded = encode(&record).expect("encode nonpositive-sequence fixture");
        store
            .connection
            .execute(
                "INSERT INTO operation_journal (operation_id, sequence, record)
                 VALUES (?1, 0, ?2)",
                params![record.operation_id.to_string(), encoded],
            )
            .expect("insert nonpositive sequence fixture");
        let error = store
            .scan_recovery_page(None)
            .expect_err("reject nonpositive sequence as scan-level audit");
        assert!(
            matches!(error, RecoveryScanError::RequiresAudit { reason } if reason.contains("non-positive"))
        );
    }

    #[test]
    fn oversized_sql_key_stops_with_a_bounded_scan_diagnostic() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let path = fixture
            .owner
            .profile_root()
            .join(super::super::JOURNAL_DATABASE_ROOT_NAME)
            .join(super::super::JOURNAL_DATABASE_FILE_NAME);
        drop(store);

        let key = "k".repeat(MAX_RECOVERY_BYTES_PER_PAGE);
        let connection = rusqlite::Connection::open(&path).expect("open fixture database");
        connection
            .execute(
                "INSERT INTO operation_journal (operation_id, sequence, record)
                 VALUES (?1, 1, zeroblob(1))",
                params![key],
            )
            .expect("insert oversized key fixture");
        drop(connection);

        let store = OperationJournalStore::open(&fixture.owner).expect("reopen store");
        let error = store
            .scan_recovery_page(None)
            .expect_err("stop before materializing oversized key");
        assert!(
            matches!(error, RecoveryScanError::RequiresAudit { reason } if reason.contains("configured recovery bound"))
        );
    }

    #[test]
    fn keyset_page_does_not_scan_the_prefix_before_a_late_cursor() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let mut record = intent_record(OperationId::new());
        let mut encoded = Vec::new();
        store
            .connection
            .execute_batch("BEGIN EXCLUSIVE")
            .expect("begin keyset fixture transaction");
        for sequence in 1..=4096_i64 {
            record.operation_id = OperationId::from_uuid(Uuid::from_u128(sequence as u128));
            encoded.clear();
            encoded.extend(encode(&record).expect("encode keyset fixture"));
            insert_raw(&store, &record.operation_id.to_string(), sequence, &encoded);
        }
        store
            .connection
            .execute_batch("COMMIT")
            .expect("commit keyset fixture transaction");

        let progress = Arc::new(AtomicUsize::new(0));
        let progress_for_handler = Arc::clone(&progress);
        store.connection.progress_handler(
            100,
            Some(move || {
                progress_for_handler.fetch_add(1, Ordering::Relaxed);
                false
            }),
        );
        let page = store
            .scan_recovery_page(RecoveryCursor::new(4090))
            .expect("scan late keyset page");
        store.connection.progress_handler(0, None::<fn() -> bool>);
        assert_eq!(page.entries().len(), 6);
        assert!(page.is_done());
        assert!(
            progress.load(Ordering::Relaxed) < 200,
            "late keyset page executed too many SQLite VM progress intervals: {}",
            progress.load(Ordering::Relaxed)
        );
    }

    #[cfg(unix)]
    #[test]
    fn namespace_replacement_is_rejected_before_recovery_results() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let root = fixture.owner.profile_root().to_path_buf();
        let displaced = root.with_file_name("journal-recovery-displaced-root");
        fs::rename(&root, &displaced).expect("displace profile root");
        fs::create_dir(&root).expect("replacement profile root");
        fs::create_dir(root.join(super::super::JOURNAL_DATABASE_ROOT_NAME))
            .expect("replacement journal root");
        let error = store
            .scan_recovery_page(None)
            .expect_err("reject replaced namespace");
        assert!(matches!(
            error,
            RecoveryScanError::Store(OperationJournalStoreError::ProfileOwnership(_))
        ));
        drop(store);
        fs::remove_dir_all(&root).expect("remove replacement root");
        fs::rename(displaced, root).expect("restore profile root");
    }
}
