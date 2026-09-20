//! Bounded, read-only paging for profile-owned operation-journal recovery.
//!
//! A recovery page is an observation of rows that are already in the journal. It never updates
//! the database, opens a filesystem path from a record, or turns stored evidence into execution
//! authority. The scan uses the journal-observation sequence as a keyset cursor and keeps both row
//! count and decoded record bytes bounded.

use rusqlite::types::ValueRef;
use rusqlite::{OptionalExtension, params};
use thiserror::Error;

use super::{JOURNAL_TABLE_NAME, OperationJournalStore, OperationJournalStoreError};
use crate::operation_journal::codec::{DecodeError, StoredOperationRecord, decode};
use crate::operation_journal::full_progress_codec::{
    FullProgressCodecError, StoredFullProgressRecord, decode as decode_full_progress,
};
use crate::operation_journal::model::{MAX_RECORD_BYTES, OperationId};
use crate::operation_journal::validation::RecordConsistency;

use super::FULL_PROGRESS_TABLE_NAME;

/// Maximum number of rows returned by one recovery page.
pub const MAX_RECOVERY_ROWS_PER_PAGE: usize = 16;
/// Maximum encoded record bytes loaded by one recovery page.
pub const MAX_RECOVERY_BYTES_PER_PAGE: usize = 2 * 1024 * 1024;

const PAGE_QUERY_LIMIT: i64 = (MAX_RECOVERY_ROWS_PER_PAGE + 1) as i64;
const MAX_OPERATION_KEY_BYTES: i64 = 128;
const MAX_OPERATION_KEY_PREVIEW_BYTES: i64 = MAX_OPERATION_KEY_BYTES + 1;
const MAX_DIAGNOSTIC_CHARS: usize = 256;

/// The journal family that owns one combined-recovery row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RecoveryFamily {
    /// The legacy operation-journal record family.
    Legacy,
    /// The internal full-progress observation family.
    FullProgress,
}

/// A decoded record retained by the combined recovery scanner.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CoordinatorRecoveryRecord {
    /// A validated legacy operation record.
    Legacy(StoredOperationRecord),
    /// A validated full-progress observation record.
    FullProgress(StoredFullProgressRecord),
}

/// One family-aware combined recovery row outcome.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CoordinatorRecoveryRowOutcome {
    /// The stored observation passed its family-specific validation.
    Consistent(CoordinatorRecoveryRecord),
    /// The source row remains retained with a bounded audit reason.
    Retained(RecoveryRetentionReason),
}

/// One bounded row from the combined legacy/full recovery page.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CoordinatorRecoveryEntry {
    family: RecoveryFamily,
    key: RecoveryKey,
    sequence: i64,
    outcome: CoordinatorRecoveryRowOutcome,
}

#[allow(dead_code)]
impl CoordinatorRecoveryEntry {
    pub(crate) const fn family(&self) -> RecoveryFamily {
        self.family
    }

    pub(crate) fn key(&self) -> &RecoveryKey {
        &self.key
    }

    pub(crate) const fn sequence(&self) -> i64 {
        self.sequence
    }

    pub(crate) fn outcome(&self) -> &CoordinatorRecoveryRowOutcome {
        &self.outcome
    }
}

/// One bounded combined recovery page.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CoordinatorRecoveryPage {
    entries: Vec<CoordinatorRecoveryEntry>,
    next_cursor: Option<RecoveryCursor>,
    done: bool,
    encoded_bytes: usize,
}

/// Read-only startup projection for the profile-owned operation journal.
///
/// `Clear` means the bounded combined scan observed no rows at the instant of the scan. `Blocked`
/// retains only a bounded actionable reason; it never exposes a raw full-progress record or grants
/// recovery, admission, or execution authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OperationJournalRecoveryReadiness {
    /// No legacy or full-progress observation was found by the combined scan.
    Clear,
    /// At least one retained observation requires coordinator recovery before writable startup.
    Blocked {
        /// Bounded family, sequence, identity, and retained-status context.
        reason: String,
    },
}

#[allow(dead_code)]
impl CoordinatorRecoveryPage {
    pub(crate) fn entries(&self) -> &[CoordinatorRecoveryEntry] {
        &self.entries
    }

    pub(crate) const fn next_cursor(&self) -> Option<RecoveryCursor> {
        self.next_cursor
    }

    pub(crate) const fn is_done(&self) -> bool {
        self.done
    }

    pub(crate) const fn encoded_bytes(&self) -> usize {
        self.encoded_bytes
    }
}

/// An opaque positive journal-observation cursor returned by a recovery page.
///
/// A cursor only selects a later observation page. It is not a mutation token, admission proof,
/// or filesystem capability.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RecoveryCursor(i64);

impl RecoveryCursor {
    /// Create a cursor from a positive stored journal-observation sequence.
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
    /// The journal-observation sequence column.
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

    /// Return the positive journal-observation sequence for this row.
    pub fn sequence(&self) -> i64 {
        self.sequence
    }

    /// Return the decoded record or retained audit reason.
    pub fn outcome(&self) -> &RecoveryRowOutcome {
        &self.outcome
    }

    /// Return a compact status projection of this scanner-produced row.
    pub fn status_snapshot(&self) -> crate::operation_journal::JournalStatusSnapshot {
        crate::operation_journal::status::from_recovery_entry(self)
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
    /// Return row observations in ascending journal-observation sequence order.
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

pub(super) fn scan_coordinator_page(
    store: &OperationJournalStore,
    cursor: Option<RecoveryCursor>,
) -> Result<CoordinatorRecoveryPage, RecoveryScanError> {
    store
        .validate_namespace()
        .map_err(RecoveryScanError::Store)?;
    store
        .connection
        .execute_batch("BEGIN")
        .map_err(store_error)?;
    let result = (|| {
        store
            .validate_namespace_for_precommit()
            .map_err(RecoveryScanError::Store)?;
        scan_coordinator_page_inner(store, cursor)
    })();
    match result {
        Ok(page) => {
            #[cfg(test)]
            let _ =
                store.insert_test_directive(super::InsertTestBoundary::BeforePrecommitValidation);
            if let Err(error) = store.validate_namespace_for_precommit() {
                let _ = store.connection.execute_batch("ROLLBACK");
                return Err(RecoveryScanError::Store(error));
            }
            if let Err(error) = store.connection.execute_batch("COMMIT") {
                let _ = store.connection.execute_batch("ROLLBACK");
                return Err(store_error(error));
            }
            store
                .validate_namespace()
                .map_err(RecoveryScanError::Store)?;
            Ok(page)
        }
        Err(error) => {
            let _ = store.connection.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

pub(super) fn recovery_readiness(
    store: &OperationJournalStore,
) -> Result<OperationJournalRecoveryReadiness, RecoveryScanError> {
    let page = scan_coordinator_page(store, None)?;
    let Some(entry) = page.entries.first() else {
        return Ok(OperationJournalRecoveryReadiness::Clear);
    };
    Ok(OperationJournalRecoveryReadiness::Blocked {
        reason: bounded_reason(format_readiness_reason(entry)),
    })
}

fn format_readiness_reason(entry: &CoordinatorRecoveryEntry) -> String {
    let family = match entry.family {
        RecoveryFamily::Legacy => "legacy",
        RecoveryFamily::FullProgress => "full-progress",
    };
    let key = match &entry.key {
        RecoveryKey::OperationId(operation_id) => format!("operation {operation_id}"),
        RecoveryKey::Text(_) => String::from("noncanonical operation key"),
        RecoveryKey::Invalid { sql_type, .. } => {
            format!("invalid operation key type {sql_type}")
        }
    };
    let status = match &entry.outcome {
        CoordinatorRecoveryRowOutcome::Consistent(_) => {
            String::from("consistent observation requires coordinator recovery")
        }
        CoordinatorRecoveryRowOutcome::Retained(reason) => {
            format!("retained audit evidence: {}", retention_reason(reason))
        }
    };
    format!("{family} {key} at sequence {}: {status}", entry.sequence)
}

fn retention_reason(reason: &RecoveryRetentionReason) -> String {
    match reason {
        RecoveryRetentionReason::LegacyMissingPublicationEvidence => {
            String::from("legacy publication evidence is missing")
        }
        RecoveryRetentionReason::UnsupportedCodec { version } => {
            format!("unsupported codec schema version {version}")
        }
        RecoveryRetentionReason::Malformed { reason } => {
            format!("malformed record ({})", bounded_reason(reason))
        }
        RecoveryRetentionReason::KeyMismatch {
            record_operation_id,
        } => {
            format!("record identity mismatches SQL key ({record_operation_id})")
        }
        RecoveryRetentionReason::SizeInvalid { declared, reason } => {
            format!(
                "invalid record size {:?} ({})",
                declared,
                bounded_reason(reason)
            )
        }
        RecoveryRetentionReason::TypeInvalid {
            field,
            sql_type,
            reason,
        } => format!(
            "invalid {field:?} SQL type {sql_type} ({})",
            bounded_reason(reason)
        ),
    }
}

fn scan_coordinator_page_inner(
    store: &OperationJournalStore,
    cursor: Option<RecoveryCursor>,
) -> Result<CoordinatorRecoveryPage, RecoveryScanError> {
    let statement = match cursor {
        Some(_) => format!(
            "SELECT family, rowid,
                    typeof(operation_id), length(CAST(operation_id AS BLOB)),
                    CASE WHEN typeof(operation_id) = 'text'
                         THEN substr(CAST(operation_id AS BLOB), 1, ?3)
                         ELSE NULL END,
                    sequence, typeof(sequence),
                    typeof(record), length(record)
             FROM (
                 SELECT 0 AS family, rowid, operation_id, sequence, record
                 FROM operation_journal WHERE sequence > ?1
                 UNION ALL
                 SELECT 1 AS family, rowid, operation_id, sequence, record
                 FROM operation_journal_full_progress WHERE sequence > ?1
             )
             ORDER BY sequence ASC, rowid ASC, family ASC
             LIMIT ?2"
        ),
        None => format!(
            "SELECT family, rowid,
                    typeof(operation_id), length(CAST(operation_id AS BLOB)),
                    CASE WHEN typeof(operation_id) = 'text'
                         THEN substr(CAST(operation_id AS BLOB), 1, ?2)
                         ELSE NULL END,
                    sequence, typeof(sequence),
                    typeof(record), length(record)
             FROM (
                 SELECT 0 AS family, rowid, operation_id, sequence, record
                 FROM operation_journal
                 UNION ALL
                 SELECT 1 AS family, rowid, operation_id, sequence, record
                 FROM operation_journal_full_progress
             )
             ORDER BY sequence ASC, rowid ASC, family ASC
             LIMIT ?1"
        ),
    };
    let mut statement = store.connection.prepare(&statement).map_err(store_error)?;
    let mut query_rows = match cursor {
        Some(cursor) => statement
            .query(params![
                cursor.sequence(),
                PAGE_QUERY_LIMIT,
                MAX_OPERATION_KEY_PREVIEW_BYTES,
            ])
            .map_err(store_error)?,
        None => statement
            .query(params![PAGE_QUERY_LIMIT, MAX_OPERATION_KEY_PREVIEW_BYTES])
            .map_err(store_error)?,
    };
    let mut candidates = Vec::with_capacity(MAX_RECOVERY_ROWS_PER_PAGE + 1);
    while let Some(row) = query_rows.next().map_err(store_error)? {
        candidates.push(read_coordinator_candidate(row)?);
    }
    drop(query_rows);
    drop(statement);

    let mut previous_sequence = cursor.map(RecoveryCursor::sequence);
    for candidate in &candidates {
        if previous_sequence.is_some_and(|previous| candidate.sequence <= previous) {
            return Err(RecoveryScanError::RequiresAudit {
                reason: bounded_reason(format!(
                    "recovery sequence {} did not advance past cursor or prior row",
                    candidate.sequence
                )),
            });
        }
        ensure_sequence_unique(store, candidate.sequence)?;
        previous_sequence = Some(candidate.sequence);
    }
    if candidates.is_empty() {
        return Ok(CoordinatorRecoveryPage {
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

        let mut entry = CoordinatorRecoveryEntry {
            family: candidate.family,
            key: candidate.key.clone(),
            sequence: candidate.sequence,
            outcome: CoordinatorRecoveryRowOutcome::Retained(RecoveryRetentionReason::Malformed {
                reason: String::from("combined recovery row was not classified"),
            }),
        };

        if let Some(reason) = candidate.key_issue.as_ref() {
            entry.outcome = CoordinatorRecoveryRowOutcome::Retained(reason.clone());
            entries.push(entry);
            last_sequence = Some(candidate.sequence);
            continue;
        }

        if let RecoveryKey::OperationId(operation_id) = candidate.key {
            let (legacy_present, full_progress_present) = store
                .operation_family_presence(operation_id)
                .map_err(RecoveryScanError::Store)?;
            if legacy_present && full_progress_present {
                entry.outcome =
                    CoordinatorRecoveryRowOutcome::Retained(RecoveryRetentionReason::Malformed {
                        reason: bounded_reason(format!(
                            "operation id {operation_id} is present in both journal families"
                        )),
                    });
                entries.push(entry);
                last_sequence = Some(candidate.sequence);
                continue;
            }
        }

        if candidate.record_type != "blob" {
            entry.outcome =
                CoordinatorRecoveryRowOutcome::Retained(RecoveryRetentionReason::TypeInvalid {
                    field: RecoveryField::Record,
                    sql_type: candidate.record_type.clone(),
                    reason: String::from("record column must contain a SQLite BLOB"),
                });
            entries.push(entry);
            last_sequence = Some(candidate.sequence);
            continue;
        }

        let Some(declared_length) = candidate.record_length else {
            entry.outcome =
                CoordinatorRecoveryRowOutcome::Retained(RecoveryRetentionReason::SizeInvalid {
                    declared: None,
                    reason: String::from("record length is NULL"),
                });
            entries.push(entry);
            last_sequence = Some(candidate.sequence);
            continue;
        };
        if declared_length < 0 || declared_length as usize > MAX_RECORD_BYTES {
            entry.outcome =
                CoordinatorRecoveryRowOutcome::Retained(RecoveryRetentionReason::SizeInvalid {
                    declared: Some(declared_length),
                    reason: String::from("record length exceeds the bounded record size"),
                });
            entries.push(entry);
            last_sequence = Some(candidate.sequence);
            continue;
        }
        let declared_length = declared_length as usize;
        if encoded_bytes.saturating_add(declared_length) > MAX_RECOVERY_BYTES_PER_PAGE {
            deferred = true;
            break;
        }

        let Some(encoded) = load_coordinator_record_bytes(store, candidate)? else {
            entry.outcome =
                CoordinatorRecoveryRowOutcome::Retained(RecoveryRetentionReason::SizeInvalid {
                    declared: Some(declared_length as i64),
                    reason: String::from(
                        "record disappeared before its bounded value could be read",
                    ),
                });
            entries.push(entry);
            last_sequence = Some(candidate.sequence);
            continue;
        };
        if encoded.len() != declared_length {
            return Err(RecoveryScanError::RequiresAudit {
                reason: String::from("record length changed during bounded recovery read"),
            });
        }
        encoded_bytes = encoded_bytes.saturating_add(encoded.len());
        entry.outcome = decode_coordinator_outcome(candidate, &encoded);
        entries.push(entry);
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

    let done = !deferred && candidates.len() <= MAX_RECOVERY_ROWS_PER_PAGE;
    Ok(CoordinatorRecoveryPage {
        entries,
        next_cursor: (!done).then_some(RecoveryCursor(last_sequence)),
        done,
        encoded_bytes,
    })
}

#[derive(Clone, Debug)]
struct CoordinatorRecoveryCandidate {
    family: RecoveryFamily,
    rowid: i64,
    key: RecoveryKey,
    key_issue: Option<RecoveryRetentionReason>,
    sequence: i64,
    record_type: String,
    record_length: Option<i64>,
}

fn read_coordinator_candidate(
    row: &rusqlite::Row<'_>,
) -> Result<CoordinatorRecoveryCandidate, RecoveryScanError> {
    let family = match row.get::<_, i64>(0).map_err(store_error)? {
        0 => RecoveryFamily::Legacy,
        1 => RecoveryFamily::FullProgress,
        other => {
            return Err(RecoveryScanError::RequiresAudit {
                reason: bounded_reason(format!("unknown recovery row family {other}")),
            });
        }
    };
    let rowid = row.get(1).map_err(store_error)?;
    let key_type: String = row.get(2).map_err(store_error)?;
    let key_length: Option<i64> = row.get(3).map_err(store_error)?;
    let key_preview = match row.get_ref(4).map_err(store_error)? {
        ValueRef::Null => None,
        ValueRef::Blob(bytes) => Some(bytes.to_vec()),
        _ => {
            return Err(RecoveryScanError::RequiresAudit {
                reason: String::from("SQLite key preview returned an unexpected type"),
            });
        }
    };
    let sequence_type: String = row.get(6).map_err(store_error)?;
    if sequence_type != "integer" {
        return Err(RecoveryScanError::RequiresAudit {
            reason: bounded_reason(format!(
                "combined recovery row {rowid} has sequence SQL type {sequence_type}"
            )),
        });
    }
    let sequence: i64 = row.get(5).map_err(store_error)?;
    if sequence <= 0 {
        return Err(RecoveryScanError::RequiresAudit {
            reason: bounded_reason(format!(
                "combined recovery row {rowid} has non-positive observation sequence {sequence}"
            )),
        });
    }
    let (key, key_issue) = read_key(key_type, key_length, key_preview);
    let record_type: String = row.get(7).map_err(store_error)?;
    let record_length: Option<i64> = row.get(8).map_err(store_error)?;
    Ok(CoordinatorRecoveryCandidate {
        family,
        rowid,
        key,
        key_issue,
        sequence,
        record_type,
        record_length,
    })
}

fn load_coordinator_record_bytes(
    store: &OperationJournalStore,
    candidate: &CoordinatorRecoveryCandidate,
) -> Result<Option<Vec<u8>>, RecoveryScanError> {
    let table = match candidate.family {
        RecoveryFamily::Legacy => JOURNAL_TABLE_NAME,
        RecoveryFamily::FullProgress => FULL_PROGRESS_TABLE_NAME,
    };
    let statement = format!(
        "SELECT typeof(record), length(record),\n                CASE WHEN typeof(record) = 'blob'\n                          AND length(record) BETWEEN 0 AND ?2\n                     THEN record ELSE NULL END\n         FROM {table} WHERE rowid = ?1"
    );
    let metadata: Option<(String, Option<i64>, Option<Vec<u8>>)> = store
        .connection
        .query_row(
            &statement,
            params![candidate.rowid, MAX_RECORD_BYTES as i64],
            |row| {
                let encoded = match row.get_ref(2)? {
                    ValueRef::Blob(bytes) => Some(bytes.to_vec()),
                    ValueRef::Null => None,
                    _ => None,
                };
                Ok((row.get(0)?, row.get(1)?, encoded))
            },
        )
        .optional()
        .map_err(store_error)?;
    let Some((record_type, record_length, encoded)) = metadata else {
        return Ok(None);
    };
    if record_type != candidate.record_type || record_length != candidate.record_length {
        return Err(RecoveryScanError::RequiresAudit {
            reason: bounded_reason(format!(
                "combined recovery row {} changed while its bounded payload was read",
                candidate.rowid
            )),
        });
    }
    Ok(encoded)
}

fn decode_coordinator_outcome(
    candidate: &CoordinatorRecoveryCandidate,
    encoded: &[u8],
) -> CoordinatorRecoveryRowOutcome {
    match candidate.family {
        RecoveryFamily::Legacy => match decode_outcome(
            &RecoveryCandidate {
                rowid: candidate.rowid,
                key: candidate.key.clone(),
                key_issue: candidate.key_issue.clone(),
                sequence: candidate.sequence,
                record_type: candidate.record_type.clone(),
                record_length: candidate.record_length,
            },
            encoded,
        ) {
            RecoveryRowOutcome::Consistent(record) => {
                CoordinatorRecoveryRowOutcome::Consistent(CoordinatorRecoveryRecord::Legacy(record))
            }
            RecoveryRowOutcome::Retained(reason) => CoordinatorRecoveryRowOutcome::Retained(reason),
        },
        RecoveryFamily::FullProgress => {
            let record = match decode_full_progress(encoded) {
                Ok(record) => record,
                Err(FullProgressCodecError::UnsupportedVersion { version }) => {
                    return CoordinatorRecoveryRowOutcome::Retained(
                        RecoveryRetentionReason::UnsupportedCodec {
                            version: u32::try_from(version).unwrap_or(u32::MAX),
                        },
                    );
                }
                Err(FullProgressCodecError::InputOversize { actual, maximum })
                | Err(FullProgressCodecError::OutputOversize { actual, maximum }) => {
                    return CoordinatorRecoveryRowOutcome::Retained(
                        RecoveryRetentionReason::SizeInvalid {
                            declared: Some(actual as i64),
                            reason: bounded_reason(format!(
                                "full-progress record size {actual} exceeds maximum {maximum}"
                            )),
                        },
                    );
                }
                Err(error) => {
                    return CoordinatorRecoveryRowOutcome::Retained(
                        RecoveryRetentionReason::Malformed {
                            reason: bounded_reason(error),
                        },
                    );
                }
            };
            if let RecoveryKey::OperationId(operation_id) = candidate.key {
                if record.snapshot().intent().operation_id() != operation_id {
                    return CoordinatorRecoveryRowOutcome::Retained(
                        RecoveryRetentionReason::KeyMismatch {
                            record_operation_id: record.snapshot().intent().operation_id(),
                        },
                    );
                }
            }
            CoordinatorRecoveryRowOutcome::Consistent(CoordinatorRecoveryRecord::FullProgress(
                record,
            ))
        }
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
                "journal row {rowid} has non-positive observation sequence {sequence}"
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
            "SELECT sequence FROM operation_journal
             WHERE typeof(sequence) = 'integer' AND sequence = ?1
             UNION ALL
             SELECT sequence FROM operation_journal_full_progress
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
                reason: bounded_reason(format!(
                    "duplicate recovery observation sequence {sequence}"
                )),
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
    use crate::operation_journal::full_progress_codec::{
        StoredFullProgressRecord, encode as encode_full_progress,
    };
    use crate::operation_journal::{
        CancellationRequestObservation, MAX_STATUS_DIAGNOSTIC_BYTES, OperationDisposition,
        OperationPhase, RetainedJournalStatus, SCHEMA_V4,
    };
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

    fn universal_intent_record(operation_id: OperationId) -> StoredOperationRecord {
        let value = serde_json::json!({
            "schema_version": 3,
            "operation_id": operation_id.as_uuid(),
            "intent": {
                "actor": "User",
                "kind": "FileMutation",
                "label": "recovery-v3-test"
            },
            "universal_intent": {
                "parent_operation_id": null,
                "command_kind": "CreateFile",
                "user_status_key": "operation.status.recovery-v3",
                "participants": {
                    "descriptors": [{
                        "id": "fedcba98-7654-3210-fedc-ba9876543210",
                        "kind": "GlobalLibrary",
                        "requirement": "Required",
                        "source_fence": null,
                        "artifact_key": null
                    }],
                    "checkpoints": [{
                        "participant_id": "fedcba98-7654-3210-fedc-ba9876543210",
                        "state": "Pending",
                        "committed_revision": null,
                        "diagnostic_code": null
                    }]
                }
            },
            "phase": "IntentDurable",
            "disposition": "None",
            "payload": {"recovery": "v3"},
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
            "created_unix_ms": 51,
            "updated_unix_ms": 52
        });
        decode(&serde_json::to_vec(&value).expect("serialize universal intent fixture"))
            .expect("decode universal intent fixture")
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

    fn insert_full_raw(store: &OperationJournalStore, key: &str, sequence: i64, record: &[u8]) {
        store
            .connection
            .execute(
                "INSERT INTO operation_journal_full_progress (operation_id, sequence, record)
                 VALUES (?1, ?2, ?3)",
                params![key, sequence, record],
            )
            .expect("insert full-progress recovery fixture row");
    }

    fn recovery_key(entry: &RecoveryEntry) -> OperationId {
        match entry.key() {
            RecoveryKey::OperationId(operation_id) => *operation_id,
            other => panic!("expected operation key, found {other:?}"),
        }
    }

    fn full_progress_fixture() -> StoredFullProgressRecord {
        let (snapshot, _) =
            crate::operation_journal::work_progress::test_full_progress_store_snapshots();
        StoredFullProgressRecord::new(snapshot).expect("full-progress recovery fixture")
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
    fn recovery_readiness_is_clear_for_an_empty_combined_journal() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        assert_eq!(
            store.recovery_readiness().expect("empty readiness"),
            OperationJournalRecoveryReadiness::Clear
        );
    }

    #[test]
    fn recovery_readiness_blocks_a_consistent_full_progress_row() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let full = full_progress_fixture();
        let operation_id = full.snapshot().intent().operation_id();
        let encoded = encode_full_progress(&full).expect("full-progress fixture bytes");
        insert_full_raw(&store, &operation_id.to_string(), 1, &encoded);

        let readiness = store.recovery_readiness().expect("full readiness");
        let OperationJournalRecoveryReadiness::Blocked { reason } = readiness else {
            panic!("full-progress row must block startup");
        };
        assert!(reason.contains("full-progress"));
        assert!(reason.contains(&operation_id.to_string()));
        assert!(reason.contains("sequence 1"));
        assert!(reason.contains("consistent observation requires coordinator recovery"));
    }

    #[test]
    fn recovery_readiness_blocks_and_bounds_a_retained_full_progress_row() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let full = full_progress_fixture();
        let operation_id = full.snapshot().intent().operation_id();
        insert_full_raw(&store, &operation_id.to_string(), 1, b"not-json");

        let readiness = store.recovery_readiness().expect("retained readiness");
        let OperationJournalRecoveryReadiness::Blocked { reason } = readiness else {
            panic!("retained full-progress row must block startup");
        };
        assert!(reason.contains("full-progress"));
        assert!(reason.contains(&operation_id.to_string()));
        assert!(reason.contains("sequence 1"));
        assert!(reason.contains("retained audit evidence"));
        assert!(reason.contains("malformed"));
        assert!(reason.len() <= 256);
    }

    #[test]
    fn checked_full_progress_test_fixture_decodes_and_validates() {
        let encoded = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../test-fixtures/full_progress_record.json"
        ));
        let full = decode_full_progress(encoded).expect("full-progress test fixture");
        full.snapshot()
            .validate_base()
            .expect("full-progress test fixture validates");
    }

    #[test]
    fn recovery_scan_classifies_v3_intent_as_consistent_observation() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let record = universal_intent_record(OperationId::new());
        store.insert_intent(&record).expect("insert v3 intent");

        let page = store.scan_recovery_page(None).expect("scan v3 intent");
        assert!(page.is_done());
        assert_eq!(page.entries().len(), 1);
        assert_eq!(page.entries()[0].sequence(), 1);
        assert_eq!(
            page.entries()[0].outcome(),
            &RecoveryRowOutcome::Consistent(record)
        );
    }

    #[test]
    fn recovery_cursor_reobserves_a_cancellation_row_after_its_sequence_moves() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let record = universal_intent_record(OperationId::new());
        let inserted = store.insert_intent(&record).expect("insert v3 intent");
        let cursor = RecoveryCursor::new(inserted.sequence()).expect("positive recovery cursor");
        let observation = CancellationRequestObservation::new(53).expect("request timestamp");
        let cancellation = store
            .request_cancel_before_publish(record.operation_id, inserted.sequence(), observation)
            .expect("persist cancellation request");
        assert_eq!(cancellation.sequence(), inserted.sequence() + 1);

        let page = store
            .scan_recovery_page(Some(cursor))
            .expect("scan changed row after old cursor");
        assert!(page.is_done());
        assert_eq!(page.entries().len(), 1);
        assert_eq!(page.entries()[0].sequence(), cancellation.sequence());
        assert!(matches!(
            page.entries()[0].outcome(),
            RecoveryRowOutcome::Consistent(observed)
                if observed.schema_version == SCHEMA_V4
                    && observed.cancellation_request() == Some(&observation)
        ));
    }

    #[test]
    fn unsupported_v3_phase_is_retained_unchanged_without_hiding_later_intents() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let record = universal_intent_record(OperationId::new());
        let mut value: serde_json::Value =
            serde_json::from_slice(&encode(&record).expect("encode V3 intent"))
                .expect("parse fixture");
        value["phase"] = serde_json::Value::from("Prepared");
        let retained = serde_json::to_vec(&value).expect("encode unsupported phase");
        insert_raw(&store, &record.operation_id.to_string(), 1, &retained);
        let later = universal_intent_record(OperationId::new());
        store.insert_intent(&later).expect("insert later intent");

        let page = store
            .scan_recovery_page(None)
            .expect("scan retained V3 row");
        assert!(page.is_done());
        assert_eq!(page.entries().len(), 2);
        assert!(matches!(
            page.entries()[0].outcome(),
            RecoveryRowOutcome::Retained(RecoveryRetentionReason::Malformed { .. })
        ));
        assert_eq!(
            page.entries()[1].outcome(),
            &RecoveryRowOutcome::Consistent(later)
        );
        let unchanged: Vec<u8> = store
            .connection
            .query_row(
                "SELECT record FROM operation_journal WHERE operation_id = ?1",
                [record.operation_id.to_string()],
                |row| row.get(0),
            )
            .expect("read retained bytes");
        assert_eq!(unchanged, retained);
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
    fn combined_recovery_orders_legacy_and_full_rows_across_cursor_pages() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        for _ in 0..16 {
            store
                .insert_intent(&intent_record(OperationId::new()))
                .expect("insert legacy page row");
        }
        let full = full_progress_fixture();
        let full_id = full.snapshot().intent().operation_id();
        assert_eq!(
            store
                .persist_full(&full, None)
                .expect("insert full page row")
                .sequence(),
            17
        );

        let first = store
            .scan_coordinator_recovery_page(None)
            .expect("scan combined first page");
        assert_eq!(first.entries().len(), MAX_RECOVERY_ROWS_PER_PAGE);
        assert!(!first.is_done());
        assert_eq!(first.next_cursor().map(RecoveryCursor::sequence), Some(16));
        assert!(
            first
                .entries()
                .iter()
                .all(|entry| entry.family() == RecoveryFamily::Legacy)
        );

        let second = store
            .scan_coordinator_recovery_page(first.next_cursor())
            .expect("scan combined second page");
        assert!(second.is_done());
        assert_eq!(second.entries().len(), 1);
        assert_eq!(second.entries()[0].family(), RecoveryFamily::FullProgress);
        assert_eq!(second.entries()[0].sequence(), 17);
        assert!(matches!(
            second.entries()[0].outcome(),
            CoordinatorRecoveryRowOutcome::Consistent(
                CoordinatorRecoveryRecord::FullProgress(record)
            ) if record.snapshot().intent().operation_id() == full_id
        ));
    }

    #[test]
    fn combined_recovery_retains_full_corruption_and_cross_family_ambiguity() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let malformed_id = OperationId::new();
        insert_full_raw(&store, &malformed_id.to_string(), 1, b"not-json");

        let ambiguous = full_progress_fixture();
        let ambiguous_id = ambiguous.snapshot().intent().operation_id();
        let encoded = encode_full_progress(&ambiguous).expect("encode ambiguous full row");
        insert_raw(
            &store,
            &ambiguous_id.to_string(),
            2,
            &encode(&intent_record(ambiguous_id)).expect("encode ambiguous legacy row"),
        );
        insert_full_raw(&store, &ambiguous_id.to_string(), 3, &encoded);

        let page = store
            .scan_coordinator_recovery_page(None)
            .expect("scan retained combined rows");
        assert_eq!(page.entries().len(), 3);
        assert!(matches!(
            page.entries()[0].outcome(),
            CoordinatorRecoveryRowOutcome::Retained(
                RecoveryRetentionReason::Malformed { reason }
            ) if reason.contains("malformed full-progress")
        ));
        for entry in &page.entries()[1..] {
            assert!(matches!(
                entry.outcome(),
                CoordinatorRecoveryRowOutcome::Retained(
                    RecoveryRetentionReason::Malformed { reason }
                ) if reason.contains("both journal families")
            ));
        }
        assert!(page.entries()[0].family() == RecoveryFamily::FullProgress);
        assert!(page.entries()[1].family() == RecoveryFamily::Legacy);
        assert!(page.entries()[2].family() == RecoveryFamily::FullProgress);
    }

    #[test]
    fn combined_recovery_rejects_duplicate_sequence_beyond_first_page() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        for sequence in 1..=17_i64 {
            let record = intent_record(OperationId::new());
            insert_raw(
                &store,
                &record.operation_id.to_string(),
                sequence,
                &encode(&record).expect("encode duplicate-page legacy row"),
            );
        }
        let full = full_progress_fixture();
        insert_full_raw(
            &store,
            &full.snapshot().intent().operation_id().to_string(),
            17,
            &encode_full_progress(&full).expect("encode duplicate-page full row"),
        );

        let error = store
            .scan_coordinator_recovery_page(None)
            .expect_err("duplicate sequence must stop combined scan");
        assert!(matches!(
            error,
            RecoveryScanError::RequiresAudit { reason }
                if reason.contains("duplicate recovery observation sequence 17")
        ));
    }

    #[test]
    fn combined_recovery_rejects_invalid_sequence_candidates_before_cursoring() {
        let first_fixture = fixture();
        let store = OperationJournalStore::open(&first_fixture.owner).expect("open store");
        let text_id = OperationId::new();
        let full = full_progress_fixture();
        insert_full_raw(
            &store,
            &text_id.to_string(),
            1,
            &encode_full_progress(&full).expect("encode invalid-sequence full row"),
        );
        store
            .connection
            .execute(
                "UPDATE operation_journal_full_progress
                 SET sequence = 'not-a-sequence' WHERE operation_id = ?1",
                params![text_id.to_string()],
            )
            .expect("corrupt full sequence type");
        let error = store
            .scan_coordinator_recovery_page(None)
            .expect_err("reject noninteger combined sequence");
        assert!(matches!(
            error,
            RecoveryScanError::RequiresAudit { reason }
                if reason.contains("sequence SQL type")
        ));

        let second_fixture = fixture();
        let store = OperationJournalStore::open(&second_fixture.owner).expect("open second store");
        let nonpositive_id = OperationId::new();
        insert_full_raw(
            &store,
            &nonpositive_id.to_string(),
            0,
            &encode_full_progress(&full).expect("encode nonpositive full row"),
        );
        let error = store
            .scan_coordinator_recovery_page(None)
            .expect_err("reject nonpositive combined sequence");
        assert!(matches!(
            error,
            RecoveryScanError::RequiresAudit { reason }
                if reason.contains("non-positive observation sequence")
        ));
    }

    #[test]
    fn combined_recovery_retains_full_unsupported_type_oversize_and_key_mismatch_rows() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let unsupported_id = OperationId::new();
        insert_full_raw(
            &store,
            &unsupported_id.to_string(),
            1,
            br#"{"record_kind":"full_progress","schema_version":999}"#,
        );
        let malformed_id = OperationId::new();
        insert_full_raw(&store, &malformed_id.to_string(), 2, b"not-json");
        let text_id = OperationId::new();
        store
            .connection
            .execute(
                "INSERT INTO operation_journal_full_progress (operation_id, sequence, record)
                 VALUES (?1, 3, 'not-a-blob')",
                params![text_id.to_string()],
            )
            .expect("insert full text record");
        let oversized_id = OperationId::new();
        store
            .connection
            .pragma_update(None, "ignore_check_constraints", true)
            .expect("disable check constraints");
        store
            .connection
            .execute(
                "INSERT INTO operation_journal_full_progress (operation_id, sequence, record)
                 VALUES (?1, 4, zeroblob(?2))",
                params![oversized_id.to_string(), (MAX_RECORD_BYTES + 1) as i64],
            )
            .expect("insert oversized full record");
        store
            .connection
            .pragma_update(None, "ignore_check_constraints", false)
            .expect("restore check constraints");
        let mismatch = full_progress_fixture();
        let mismatch_key = OperationId::new();
        let mismatch_bytes = encode_full_progress(&mismatch).expect("encode mismatch full row");
        insert_full_raw(&store, &mismatch_key.to_string(), 5, &mismatch_bytes);

        let page = store
            .scan_coordinator_recovery_page(None)
            .expect("scan retained full rows");
        assert_eq!(page.entries().len(), 5);
        assert!(matches!(
            page.entries()[0].outcome(),
            CoordinatorRecoveryRowOutcome::Retained(RecoveryRetentionReason::UnsupportedCodec {
                version: 999
            })
        ));
        assert!(matches!(
            page.entries()[1].outcome(),
            CoordinatorRecoveryRowOutcome::Retained(RecoveryRetentionReason::Malformed { .. })
        ));
        assert!(matches!(
            page.entries()[2].outcome(),
            CoordinatorRecoveryRowOutcome::Retained(
                RecoveryRetentionReason::TypeInvalid {
                    field: RecoveryField::Record,
                    sql_type,
                    ..
                }
            ) if sql_type == "text"
        ));
        assert!(matches!(
            page.entries()[3].outcome(),
            CoordinatorRecoveryRowOutcome::Retained(RecoveryRetentionReason::SizeInvalid { .. })
        ));
        assert!(matches!(
            page.entries()[4].outcome(),
            CoordinatorRecoveryRowOutcome::Retained(
                RecoveryRetentionReason::KeyMismatch { record_operation_id }
            ) if *record_operation_id == mismatch.snapshot().intent().operation_id()
        ));
        assert!(
            page.entries()
                .iter()
                .all(|entry| entry.family() == RecoveryFamily::FullProgress)
        );
    }

    #[test]
    fn combined_recovery_applies_encoded_byte_budget_across_both_families() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let legacy = intent_record(OperationId::new());
        let mut legacy_bytes = encode(&legacy).expect("encode large legacy row");
        legacy_bytes.resize(MAX_RECORD_BYTES, b' ');
        insert_raw(&store, &legacy.operation_id.to_string(), 1, &legacy_bytes);
        let full = full_progress_fixture();
        let mut full_bytes = encode_full_progress(&full).expect("encode large full row");
        full_bytes.resize(MAX_RECORD_BYTES, b' ');
        insert_full_raw(
            &store,
            &full.snapshot().intent().operation_id().to_string(),
            2,
            &full_bytes,
        );
        let small = intent_record(OperationId::new());
        store
            .insert_intent(&small)
            .expect("insert deferred small row");

        let first = store
            .scan_coordinator_recovery_page(None)
            .expect("scan cross-family byte budget");
        assert_eq!(first.entries().len(), 2);
        assert!(!first.is_done());
        assert_eq!(first.encoded_bytes(), MAX_RECOVERY_BYTES_PER_PAGE);
        assert_eq!(first.next_cursor().map(RecoveryCursor::sequence), Some(2));
        let second = store
            .scan_coordinator_recovery_page(first.next_cursor())
            .expect("scan deferred cross-family row");
        assert!(second.is_done());
        assert_eq!(second.entries().len(), 1);
        assert_eq!(second.entries()[0].sequence(), 3);
    }

    #[test]
    fn combined_recovery_reopens_with_the_same_mixed_observations() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let legacy = intent_record(OperationId::new());
        store.insert_intent(&legacy).expect("insert legacy row");
        let full = full_progress_fixture();
        store.persist_full(&full, None).expect("insert full row");
        drop(store);

        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen store");
        let page = reopened
            .scan_coordinator_recovery_page(None)
            .expect("scan reopened mixed rows");
        assert_eq!(page.entries().len(), 2);
        assert_eq!(page.entries()[0].family(), RecoveryFamily::Legacy);
        assert_eq!(page.entries()[1].family(), RecoveryFamily::FullProgress);
        assert!(matches!(
            page.entries()[1].outcome(),
            CoordinatorRecoveryRowOutcome::Consistent(
                CoordinatorRecoveryRecord::FullProgress(observed)
            ) if observed == &full
        ));
    }

    #[test]
    fn combined_recovery_guard_failure_releases_transaction_and_preserves_rows() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let legacy = intent_record(OperationId::new());
        let legacy_id = legacy.operation_id;
        store.insert_intent(&legacy).expect("insert legacy row");
        let full = full_progress_fixture();
        let full_id = full.snapshot().intent().operation_id();
        store.persist_full(&full, None).expect("insert full row");

        let legacy_raw: (String, i64, Vec<u8>) = store
            .connection
            .query_row(
                "SELECT operation_id, sequence, record
                 FROM operation_journal WHERE operation_id = ?1",
                params![legacy_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read original legacy row");
        let full_raw: (String, i64, Vec<u8>) = store
            .connection
            .query_row(
                "SELECT operation_id, sequence, record
                 FROM operation_journal_full_progress WHERE operation_id = ?1",
                params![full_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read original full row");

        let sidecar = fixture
            .owner
            .profile_root()
            .join(super::super::JOURNAL_DATABASE_ROOT_NAME)
            .join("journal.sqlite3-wal");
        let marker = b"unexpected combined-recovery WAL".to_vec();
        let sidecar_for_hook = sidecar.clone();
        let marker_for_hook = marker.clone();
        let mut injected = false;
        store.set_insert_test_hook(move |boundary| {
            if boundary == super::super::InsertTestBoundary::BeforePrecommitValidation && !injected
            {
                fs::write(&sidecar_for_hook, &marker_for_hook)
                    .expect("inject combined-recovery WAL sidecar");
                injected = true;
            }
            super::super::InsertTestDirective::Continue
        });

        let error = store
            .scan_coordinator_recovery_page(None)
            .expect_err("unexpected sidecar must reject combined recovery");
        assert!(matches!(
            error,
            RecoveryScanError::Store(OperationJournalStoreError::RequiresAudit { reason, .. })
                if reason.contains("sidecar")
        ));
        assert!(
            store.connection.is_autocommit(),
            "combined recovery must release its read transaction on guard failure"
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT operation_id, sequence, record
                     FROM operation_journal WHERE operation_id = ?1",
                    params![legacy_id.to_string()],
                    |row| Ok((row.get::<_, String>(0)?, row.get(1)?, row.get(2)?)),
                )
                .expect("read legacy row after rollback"),
            legacy_raw
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT operation_id, sequence, record
                     FROM operation_journal_full_progress WHERE operation_id = ?1",
                    params![full_id.to_string()],
                    |row| Ok((row.get::<_, String>(0)?, row.get(1)?, row.get(2)?)),
                )
                .expect("read full row after rollback"),
            full_raw
        );
        assert_eq!(
            fs::read(&sidecar).expect("retain injected sidecar evidence"),
            marker
        );
        fs::remove_file(&sidecar).expect("remove retained test sidecar");

        let page = store
            .scan_coordinator_recovery_page(None)
            .expect("scan succeeds after guard evidence is removed");
        assert_eq!(page.entries().len(), 2);
        assert!(page.is_done());
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
    fn status_snapshot_retains_audit_reasons_and_never_projects_invalid_success() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");

        let record_id = OperationId::new();
        let key_id = OperationId::new();
        let record = intent_record(record_id);
        insert_raw(
            &store,
            &key_id.to_string(),
            1,
            &encode(&record).expect("encode mismatch fixture"),
        );

        let malformed_id = OperationId::new();
        let private_marker = "/private/marker";
        let malformed = format!(r#"{{"schema_version":3,"{private_marker}":1}}"#);
        insert_raw(&store, &malformed_id.to_string(), 2, malformed.as_bytes());

        let unsupported_id = OperationId::new();
        insert_raw(
            &store,
            &unsupported_id.to_string(),
            3,
            br#"{"schema_version":999}"#,
        );

        let invalid_terminal_id = OperationId::new();
        let mut invalid_terminal = intent_record(invalid_terminal_id);
        invalid_terminal.phase = OperationPhase::Terminal;
        invalid_terminal.disposition = OperationDisposition::Succeeded;
        insert_raw(
            &store,
            &invalid_terminal_id.to_string(),
            4,
            &encode(&invalid_terminal).expect("encode terminal fixture"),
        );

        let page = store.scan_recovery_page(None).expect("scan retained rows");
        assert_eq!(page.entries().len(), 4);

        let mismatch = page.entries()[0].status_snapshot();
        assert_eq!(mismatch.operation_id(), Some(key_id));
        assert!(mismatch.observed().is_none());
        assert!(matches!(
            mismatch.retained().map(RetainedJournalStatus::reason),
            Some(RecoveryRetentionReason::KeyMismatch {
                record_operation_id
            }) if *record_operation_id == record_id
        ));

        let malformed = page.entries()[1].status_snapshot();
        assert!(matches!(
            malformed.retained().map(RetainedJournalStatus::reason),
            Some(RecoveryRetentionReason::Malformed { .. })
        ));
        assert!(
            malformed
                .retained()
                .expect("malformed retention")
                .diagnostic()
                .len()
                <= MAX_STATUS_DIAGNOSTIC_BYTES
        );
        assert!(
            !malformed
                .retained()
                .expect("malformed retention")
                .diagnostic()
                .contains(private_marker)
        );
        assert!(!format!("{malformed:?}").contains(private_marker));
        assert!(matches!(
            malformed.retained().map(RetainedJournalStatus::reason),
            Some(RecoveryRetentionReason::Malformed { reason })
                if !reason.contains(private_marker)
        ));

        let unsupported = page.entries()[2].status_snapshot();
        assert!(matches!(
            unsupported.retained().map(RetainedJournalStatus::reason),
            Some(RecoveryRetentionReason::UnsupportedCodec { version: 999 })
        ));

        let invalid_terminal = page.entries()[3].status_snapshot();
        assert!(invalid_terminal.observed().is_none());
        assert!(matches!(
            invalid_terminal
                .retained()
                .map(RetainedJournalStatus::reason),
            Some(RecoveryRetentionReason::LegacyMissingPublicationEvidence)
        ));
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
        let invalid_key_status = page.entries()[1].status_snapshot();
        assert!(matches!(
            invalid_key_status.key(),
            RecoveryKey::Invalid { preview, .. }
                if preview == "operation key retained for audit"
        ));
        assert!(!format!("{invalid_key_status:?}").contains("not-an-operation-id"));
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
