//! Bounded, read-only paging for the legacy source-local file-operation journal.
//!
//! The source-local journal predates the profile operation journal and has no observation
//! sequence.  This reader therefore uses a temporary SQLite rowid cursor and deliberately keeps
//! its result separate from the universal operation-journal recovery types.  A decoded legacy row
//! is evidence for a later compatibility decision; it is never an execution-ready operation.

use std::fmt::Display;

use blake3::Hasher;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use thiserror::Error;

use super::super::{SourceDatabase, SourceDbError, SourcePublicationFence};
use super::decode::{MalformedJournalEntry, decode_journal_row};
use super::entry::FileOpJournalEntry;

/// Maximum number of legacy journal rows returned by one page.
pub const MAX_LEGACY_JOURNAL_ROWS_PER_PAGE: usize = 16;
/// Maximum number of bounded legacy fingerprint/value bytes materialized by one page.
pub const MAX_LEGACY_JOURNAL_BYTES_PER_PAGE: usize = 2 * 1024 * 1024;
/// Maximum number of bytes materialized from one legacy text field.
pub const MAX_LEGACY_JOURNAL_FIELD_BYTES: usize = 64 * 1024;
/// Maximum number of bytes retained in one legacy key or diagnostic preview.
pub const MAX_LEGACY_JOURNAL_PREVIEW_BYTES: usize = 256;

const PAGE_QUERY_LIMIT: i64 = (MAX_LEGACY_JOURNAL_ROWS_PER_PAGE + 1) as i64;
const LEGACY_COLUMN_COUNT: usize = 18;
const MAX_LEGACY_JOURNAL_SCHEMA_COLUMNS: usize = 32;
const MAX_LEGACY_JOURNAL_SCHEMA_NAME_BYTES: usize = 256;
const LEGACY_DEDUP_FINGERPRINT_VERSION: u8 = 1;
const LEGACY_DEDUP_FINGERPRINT_DOMAIN: &[u8] = b"yield.legacy-journal.dedup\0";

const LEGACY_COLUMNS: [&str; LEGACY_COLUMN_COUNT] = [
    "id",
    "op_type",
    "stage",
    "source_root",
    "source_relative",
    "target_relative",
    "staged_relative",
    "file_size",
    "modified_ns",
    "tag",
    "looped",
    "locked",
    "last_played_at",
    "last_curated_at",
    "created_at",
    "file_identity",
    "target_root_identity",
    "source_root_identity",
];

/// The field whose legacy SQLite representation was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LegacyJournalField {
    /// Stable legacy journal identifier.
    Id,
    /// Stored operation kind (`copy` or `move`).
    OperationType,
    /// Stored recovery stage.
    Stage,
    /// Absolute source root used by legacy moves.
    SourceRoot,
    /// Source-relative path used by legacy moves.
    SourceRelative,
    /// Target-relative path.
    TargetRelative,
    /// Staged-relative path.
    StagedRelative,
    /// Recorded file length.
    FileSize,
    /// Recorded file modification timestamp.
    ModifiedNs,
    /// Stored rating.
    Tag,
    /// Stored loop marker.
    Looped,
    /// Stored lock marker.
    Locked,
    /// Stored playback timestamp.
    LastPlayedAt,
    /// Stored curation timestamp.
    LastCuratedAt,
    /// Journal creation timestamp.
    CreatedAt,
    /// Recorded filesystem object identity.
    FileIdentity,
    /// Recorded target-root identity.
    TargetRootIdentity,
    /// Recorded move-source-root identity.
    SourceRootIdentity,
    /// The complete row exceeded the page's bounded materialization budget.
    Row,
}

impl LegacyJournalField {
    fn from_index(index: usize) -> Self {
        match index {
            0 => Self::Id,
            1 => Self::OperationType,
            2 => Self::Stage,
            3 => Self::SourceRoot,
            4 => Self::SourceRelative,
            5 => Self::TargetRelative,
            6 => Self::StagedRelative,
            7 => Self::FileSize,
            8 => Self::ModifiedNs,
            9 => Self::Tag,
            10 => Self::Looped,
            11 => Self::Locked,
            12 => Self::LastPlayedAt,
            13 => Self::LastCuratedAt,
            14 => Self::CreatedAt,
            15 => Self::FileIdentity,
            16 => Self::TargetRootIdentity,
            17 => Self::SourceRootIdentity,
            _ => Self::Row,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Id => "id",
            Self::OperationType => "op_type",
            Self::Stage => "stage",
            Self::SourceRoot => "source_root",
            Self::SourceRelative => "source_relative",
            Self::TargetRelative => "target_relative",
            Self::StagedRelative => "staged_relative",
            Self::FileSize => "file_size",
            Self::ModifiedNs => "modified_ns",
            Self::Tag => "tag",
            Self::Looped => "looped",
            Self::Locked => "locked",
            Self::LastPlayedAt => "last_played_at",
            Self::LastCuratedAt => "last_curated_at",
            Self::CreatedAt => "created_at",
            Self::FileIdentity => "file_identity",
            Self::TargetRootIdentity => "target_root_identity",
            Self::SourceRootIdentity => "source_root_identity",
            Self::Row => "row",
        }
    }

    fn is_text(self) -> bool {
        matches!(
            self,
            Self::Id
                | Self::OperationType
                | Self::Stage
                | Self::SourceRoot
                | Self::SourceRelative
                | Self::TargetRelative
                | Self::StagedRelative
                | Self::FileIdentity
                | Self::TargetRootIdentity
                | Self::SourceRootIdentity
        )
    }

    fn allows_null(self) -> bool {
        !matches!(
            self,
            Self::Id | Self::OperationType | Self::Stage | Self::TargetRelative | Self::CreatedAt
        )
    }
}

/// The bounded key observation attached to one legacy row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LegacyJournalKey {
    /// A complete, bounded legacy id.
    Text(String),
    /// A bounded prefix of a valid text id whose complete value exceeds the key preview bound.
    TextPreview(String),
    /// The id column had an unexpected SQL representation.
    Invalid {
        /// SQLite's reported type.
        sql_type: String,
        /// Bounded value preview, when one was available.
        preview: String,
    },
}

/// A typed reason why a legacy journal row remains retained for audit or later qualification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LegacyJournalRetentionReason {
    /// A required field or a legacy path/value relationship was malformed.
    Malformed {
        /// Bounded id preview when one was available.
        id: Option<String>,
        /// Bounded decoder diagnostic.
        detail: String,
    },
    /// The operation kind is not supported by the legacy decoder.
    UnknownOperationType {
        /// Bounded id preview when one was available.
        id: Option<String>,
        /// Bounded stored value.
        value: String,
    },
    /// The recovery stage is not supported by the legacy decoder.
    UnknownStage {
        /// Bounded id preview when one was available.
        id: Option<String>,
        /// Bounded stored value.
        value: String,
    },
    /// A field had a SQLite type that does not match the legacy schema contract.
    TypeInvalid {
        /// Rejected field.
        field: LegacyJournalField,
        /// SQLite's reported type.
        sql_type: String,
        /// Bounded value preview.
        preview: String,
    },
    /// A text field or row exceeded the bounded reader contract.
    SizeInvalid {
        /// Rejected field, or [`LegacyJournalField::Row`] for an aggregate bound.
        field: LegacyJournalField,
        /// SQLite's declared byte length, when available.
        declared: Option<i64>,
        /// Bounded diagnostic.
        reason: String,
    },
    /// The row disappeared or changed between bounded metadata and value reads.
    RowChanged {
        /// SQLite rowid observed by the page.
        rowid: i64,
        /// Bounded diagnostic.
        reason: String,
    },
    /// The candidate disappeared before its bounded value could be read.
    RowDisappeared {
        /// SQLite rowid observed by the page.
        rowid: i64,
    },
}

/// Outcome for one row in a bounded legacy page.
#[derive(Clone, Debug)]
pub enum LegacyJournalRowOutcome {
    /// The legacy row decoded under the bounded legacy codec.
    ///
    /// This is an observation only.  It is not a universal operation record and does not grant
    /// filesystem, source, capacity, or execution authority.
    Decoded(FileOpJournalEntry),
    /// The row remains retained and unchanged for a later audit or compatibility decision.
    Retained(LegacyJournalRetentionReason),
}

/// A complete, versioned identity for one raw legacy journal row.
///
/// The source and database identities come from the retained bindings validated at the scan
/// boundary.  The digest covers every known SQLite cell in the fixed canonical column order;
/// it is not derived from a decoded entry or a bounded diagnostic preview.  Version one hashes a
/// domain marker, the version byte, and for each column its UTF-8 name length/name, storage-class
/// tag, value length, and complete raw value.  Integers and real values use fixed-width little
/// endian bytes; text and blobs retain their SQLite bytes; NULL has a distinct empty value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LegacyJournalDedupIdentity {
    root_identity: String,
    database_identity: String,
    rowid: i64,
    fingerprint_version: u8,
    fingerprint: [u8; 32],
}

impl LegacyJournalDedupIdentity {
    /// Return the verified database-root identity containing the source journal.
    pub fn root_identity(&self) -> &str {
        &self.root_identity
    }

    /// Return the verified SQLite main-file identity.
    pub fn database_identity(&self) -> &str {
        &self.database_identity
    }

    /// Return the SQLite rowid included in this identity.
    pub fn rowid(&self) -> i64 {
        self.rowid
    }

    /// Return the canonical raw-cell fingerprint version.
    pub fn fingerprint_version(&self) -> u8 {
        self.fingerprint_version
    }

    /// Return the complete BLAKE3 fingerprint of the canonical raw cells.
    pub fn fingerprint(&self) -> &[u8; 32] {
        &self.fingerprint
    }
}

/// A bounded reason why a complete legacy dedup identity could not be produced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LegacyJournalDedupIdentityUnavailable {
    /// The physical table did not expose exactly the known legacy columns.
    Schema {
        /// Bounded schema diagnostic.
        reason: String,
    },
    /// One raw SQLite value exceeded the per-cell bound.
    ValueTooLarge {
        /// Field whose complete value was not materialized.
        field: LegacyJournalField,
        /// SQLite byte length, when available.
        declared: Option<i64>,
    },
    /// A raw SQLite representation could not be fingerprinted completely.
    ValueUnavailable {
        /// Field whose raw value was unavailable.
        field: LegacyJournalField,
        /// Bounded diagnostic.
        reason: String,
    },
    /// The row changed between candidate metadata and bounded raw-cell read.
    RowChanged {
        /// Bounded diagnostic.
        reason: String,
    },
    /// The candidate row disappeared before the raw-cell read.
    RowDisappeared,
    /// The complete canonical row would exceed the page materialization bound.
    RowTooLarge {
        /// Canonical encoded byte count when it was representable.
        declared: Option<i64>,
    },
}

/// Result of the scanner's bounded attempt to produce a legacy dedup identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LegacyJournalDedupIdentityResult {
    /// A complete identity was produced from the verified source binding and raw cells.
    Complete(LegacyJournalDedupIdentity),
    /// The row remains observable, but its complete identity is unavailable.
    Unavailable(LegacyJournalDedupIdentityUnavailable),
}

/// One bounded legacy journal row observation.
#[derive(Clone, Debug)]
pub struct LegacyJournalEntry {
    rowid: i64,
    key: LegacyJournalKey,
    outcome: LegacyJournalRowOutcome,
    dedup_identity: LegacyJournalDedupIdentityResult,
    bytes: usize,
}

impl LegacyJournalEntry {
    /// Return the signed SQLite rowid.  Legacy rowids may be negative or zero.
    pub fn rowid(&self) -> i64 {
        self.rowid
    }

    /// Return the bounded legacy id observation.
    pub fn key(&self) -> &LegacyJournalKey {
        &self.key
    }

    /// Return the decoded legacy entry or retained diagnostic.
    pub fn outcome(&self) -> &LegacyJournalRowOutcome {
        &self.outcome
    }

    /// Return the complete identity or bounded unavailability reason for this row.
    pub fn dedup_identity(&self) -> &LegacyJournalDedupIdentityResult {
        &self.dedup_identity
    }

    /// Return the bounded raw/fingerprint bytes materialized for this row.
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

/// Temporary cursor for a bounded legacy page sequence.
///
/// The upper rowid is captured on the first page so rows appended later do not move the current
/// observation boundary.  This is not a durable import watermark or a consistent snapshot.  The
/// weak publication fence binds the cursor to the retained source owner without keeping its
/// source lease or SQLite control connection alive.
#[derive(Clone, Debug)]
pub struct LegacyJournalCursor {
    upper_rowid: i64,
    after_rowid: i64,
    publication_fence: SourcePublicationFence,
}

impl LegacyJournalCursor {
    /// Return the captured upper rowid for this temporary observation.
    pub fn upper_rowid(&self) -> i64 {
        self.upper_rowid
    }

    /// Return the last rowid consumed by the preceding page.
    pub fn after_rowid(&self) -> i64 {
        self.after_rowid
    }
}

/// One bounded, read-only legacy journal page.
#[derive(Clone, Debug)]
pub struct LegacyJournalPage {
    entries: Vec<LegacyJournalEntry>,
    next_cursor: Option<LegacyJournalCursor>,
    done: bool,
    bytes: usize,
}

impl LegacyJournalPage {
    /// Return row observations in signed rowid order.
    pub fn entries(&self) -> &[LegacyJournalEntry] {
        &self.entries
    }

    /// Consume the page and return its row observations.
    pub fn into_entries(self) -> Vec<LegacyJournalEntry> {
        self.entries
    }

    /// Return the cursor for another page, or `None` when the captured boundary is complete.
    pub fn next_cursor(&self) -> Option<LegacyJournalCursor> {
        self.next_cursor.clone()
    }

    /// Return whether no more rows are visible through the captured upper rowid.
    ///
    /// The cursor is not a snapshot or durable watermark, so rows already seen can still be
    /// changed or removed between pages.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Return the aggregate bounded raw/fingerprint bytes materialized for this page.
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

/// Failure while reading a bounded legacy journal page.
#[derive(Debug, Error)]
pub enum LegacyJournalScanError {
    /// The retained source database ownership or namespace could not be validated.
    #[error(transparent)]
    SourceDb(#[from] SourceDbError),
    /// The cursor no longer belongs to the retained source owner.
    #[error("legacy file-operation journal cursor is stale: {reason}")]
    StaleCursor {
        /// Bounded cursor invalidation diagnostic.
        reason: String,
    },
    /// The source journal schema or bounded query result requires audit.
    #[error("legacy file-operation journal scan requires audit: {reason}")]
    RequiresAudit {
        /// Bounded scan diagnostic.
        reason: String,
    },
}

/// Read one bounded page of legacy source-local journal rows.
///
/// The retained profile/source ownership and the bound database connection are validated before
/// and after the page.  This function never renews ownership, opens a filesystem path, mutates a
/// row, removes evidence, or invokes the legacy reconciler.
pub fn scan_page(
    db: &SourceDatabase,
    cursor: Option<LegacyJournalCursor>,
) -> Result<LegacyJournalPage, LegacyJournalScanError> {
    let before_fence = db.source_publication_fence()?;
    let source_binding = db.legacy_journal_dedup_binding()?;
    if !before_fence.is_live() {
        return Err(LegacyJournalScanError::StaleCursor {
            reason: String::from("source ownership has been retired"),
        });
    }
    if let Some(cursor) = cursor.as_ref()
        && (!cursor.publication_fence.is_live()
            || !cursor.publication_fence.same_claim_identity(&before_fence))
    {
        return Err(LegacyJournalScanError::StaleCursor {
            reason: String::from("cursor belongs to a different retained source owner"),
        });
    }

    // A deferred transaction gives the metadata preflight and bounded materialization one SQLite
    // snapshot.  It also fails when a caller already has a transaction open, rather than
    // committing or nesting over caller-owned work.  The transaction is always rolled back.
    let transaction = db
        .connection
        .unchecked_transaction()
        .map_err(scan_sql_error)?;
    let page_result = scan_page_in_transaction(
        &transaction,
        cursor.as_ref(),
        &before_fence,
        &source_binding,
    );
    let rollback_result = transaction.rollback().map_err(scan_sql_error);
    let mut page = match (page_result, rollback_result) {
        (_, Err(error)) => return Err(error),
        (Err(error), Ok(())) => return Err(error),
        (Ok(page), Ok(())) => page,
    };

    let after_fence = db.source_publication_fence()?;
    ensure_same_fence(&before_fence, &after_fence)?;
    if let Some(cursor) = page.next_cursor.as_mut() {
        cursor.publication_fence = after_fence;
    }
    Ok(page)
}

fn scan_page_in_transaction(
    transaction: &Transaction<'_>,
    cursor: Option<&LegacyJournalCursor>,
    publication_fence: &SourcePublicationFence,
    source_binding: &(String, String),
) -> Result<LegacyJournalPage, LegacyJournalScanError> {
    let (upper_rowid, after_rowid) = match cursor {
        Some(cursor) => (Some(cursor.upper_rowid), Some(cursor.after_rowid)),
        None => (load_upper_rowid(transaction)?, None),
    };
    let Some(upper_rowid) = upper_rowid else {
        return Ok(LegacyJournalPage {
            entries: Vec::new(),
            next_cursor: None,
            done: true,
            bytes: 0,
        });
    };

    let schema = load_legacy_schema(transaction)?;
    if let LegacySchemaStatus::Unavailable {
        reason,
        projection_safe: false,
    } = &schema
    {
        return Err(LegacyJournalScanError::RequiresAudit {
            reason: match reason {
                LegacyJournalDedupIdentityUnavailable::Schema { reason } => reason.clone(),
                _ => String::from("legacy journal schema cannot be safely projected"),
            },
        });
    }
    let candidates = load_candidates(transaction, upper_rowid, after_rowid)?;
    let mut entries = Vec::with_capacity(MAX_LEGACY_JOURNAL_ROWS_PER_PAGE);
    let mut bytes = 0_usize;
    let mut deferred = false;
    let mut last_rowid = after_rowid;

    for candidate in candidates.iter() {
        if entries.len() >= MAX_LEGACY_JOURNAL_ROWS_PER_PAGE {
            deferred = true;
            break;
        }

        let row_bytes = match candidate.fingerprint_bytes() {
            Ok(bytes) => bytes,
            Err(LegacyJournalDedupIdentityUnavailable::ValueTooLarge { field, declared }) => {
                last_rowid = Some(candidate.rowid);
                entries.push(LegacyJournalEntry {
                    rowid: candidate.rowid,
                    key: candidate.key(),
                    outcome: LegacyJournalRowOutcome::Retained(
                        candidate.metadata_issue().unwrap_or(
                            LegacyJournalRetentionReason::SizeInvalid {
                                field,
                                declared,
                                reason: String::from(
                                    "legacy raw value exceeds the bounded field size",
                                ),
                            },
                        ),
                    ),
                    dedup_identity: unavailable_for_schema_or_row(
                        &schema,
                        LegacyJournalDedupIdentityUnavailable::ValueTooLarge { field, declared },
                    ),
                    bytes: 0,
                });
                continue;
            }
            Err(_) => candidate.text_bytes()?,
        };
        if row_bytes > MAX_LEGACY_JOURNAL_BYTES_PER_PAGE {
            last_rowid = Some(candidate.rowid);
            entries.push(LegacyJournalEntry {
                rowid: candidate.rowid,
                key: candidate.key(),
                outcome: LegacyJournalRowOutcome::Retained(
                    LegacyJournalRetentionReason::SizeInvalid {
                        field: LegacyJournalField::Row,
                        declared: i64::try_from(row_bytes).ok(),
                        reason: String::from("row exceeds the bounded page byte budget"),
                    },
                ),
                dedup_identity: unavailable_for_schema_or_row(
                    &schema,
                    LegacyJournalDedupIdentityUnavailable::RowTooLarge {
                        declared: i64::try_from(row_bytes).ok(),
                    },
                ),
                bytes: 0,
            });
            continue;
        }
        if bytes.saturating_add(row_bytes) > MAX_LEGACY_JOURNAL_BYTES_PER_PAGE {
            deferred = true;
            break;
        }

        let loaded = load_bounded_row(transaction, candidate, source_binding, &schema)?;
        let (outcome, dedup_identity, loaded_bytes) = match loaded {
            BoundedRow::Missing { identity } => (
                LegacyJournalRowOutcome::Retained(LegacyJournalRetentionReason::RowDisappeared {
                    rowid: candidate.rowid,
                }),
                identity,
                0,
            ),
            BoundedRow::Changed { reason, identity } => (
                LegacyJournalRowOutcome::Retained(LegacyJournalRetentionReason::RowChanged {
                    rowid: candidate.rowid,
                    reason,
                }),
                identity,
                0,
            ),
            BoundedRow::Decoded {
                entry,
                identity,
                bytes,
            } => (LegacyJournalRowOutcome::Decoded(entry), identity, bytes),
            BoundedRow::Malformed {
                malformed,
                identity,
                bytes,
            } => (
                LegacyJournalRowOutcome::Retained(map_malformed(malformed)),
                identity,
                bytes,
            ),
            BoundedRow::Retained {
                reason,
                identity,
                bytes,
            } => (LegacyJournalRowOutcome::Retained(reason), identity, bytes),
        };
        bytes = bytes.saturating_add(loaded_bytes);
        last_rowid = Some(candidate.rowid);
        entries.push(LegacyJournalEntry {
            rowid: candidate.rowid,
            key: candidate.key(),
            outcome,
            dedup_identity,
            bytes: loaded_bytes,
        });
    }

    let Some(last_rowid) = last_rowid else {
        return Err(LegacyJournalScanError::RequiresAudit {
            reason: String::from("the first legacy row could not advance a signed rowid cursor"),
        });
    };
    if entries.is_empty() && deferred {
        return Err(LegacyJournalScanError::RequiresAudit {
            reason: String::from("the first legacy row cannot fit within the page bound"),
        });
    }
    let done = !deferred && candidates.len() <= MAX_LEGACY_JOURNAL_ROWS_PER_PAGE;
    let next_cursor = (!done).then_some(LegacyJournalCursor {
        upper_rowid,
        after_rowid: last_rowid,
        publication_fence: publication_fence.clone(),
    });
    Ok(LegacyJournalPage {
        entries,
        next_cursor,
        done,
        bytes,
    })
}

fn ensure_same_fence(
    before: &SourcePublicationFence,
    after: &SourcePublicationFence,
) -> Result<(), LegacyJournalScanError> {
    if before.is_live() && after.is_live() && before.same_claim_identity(after) {
        Ok(())
    } else if !before.is_live() || !after.is_live() {
        Err(LegacyJournalScanError::StaleCursor {
            reason: String::from("source ownership was retired while reading the page"),
        })
    } else {
        Err(LegacyJournalScanError::StaleCursor {
            reason: String::from("source ownership changed while reading the page"),
        })
    }
}

fn load_upper_rowid(connection: &Connection) -> Result<Option<i64>, LegacyJournalScanError> {
    connection
        .query_row(
            "SELECT rowid FROM file_ops_journal ORDER BY rowid DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(SourceDbError::from)
        .map_err(LegacyJournalScanError::from)
}

#[derive(Clone, Debug)]
enum LegacySchemaStatus {
    Complete,
    Unavailable {
        reason: LegacyJournalDedupIdentityUnavailable,
        projection_safe: bool,
    },
}

fn load_legacy_schema(
    connection: &Connection,
) -> Result<LegacySchemaStatus, LegacyJournalScanError> {
    let mut statement = connection
        .prepare("SELECT name, hidden FROM pragma_table_xinfo('file_ops_journal') ORDER BY cid")
        .map_err(SourceDbError::from)?;
    let mut rows = statement.query([]).map_err(SourceDbError::from)?;
    let mut names = Vec::with_capacity(LEGACY_COLUMN_COUNT);
    while let Some(row) = rows.next().map_err(scan_sql_error)? {
        if names.len() >= MAX_LEGACY_JOURNAL_SCHEMA_COLUMNS {
            return Ok(LegacySchemaStatus::Unavailable {
                reason: LegacyJournalDedupIdentityUnavailable::Schema {
                    reason: String::from("legacy journal schema exposes too many columns"),
                },
                projection_safe: false,
            });
        }
        let name = match row.get_ref(0).map_err(scan_sql_error)? {
            rusqlite::types::ValueRef::Text(bytes)
                if bytes.len() <= MAX_LEGACY_JOURNAL_SCHEMA_NAME_BYTES =>
            {
                String::from_utf8(bytes.to_vec()).map_err(|_| {
                    LegacyJournalScanError::RequiresAudit {
                        reason: String::from("legacy journal schema column name is not UTF-8"),
                    }
                })?
            }
            rusqlite::types::ValueRef::Text(_) => format!("__unbounded_{}", names.len()),
            _ => format!("__invalid_{}", names.len()),
        };
        let hidden: i64 = row.get(1).map_err(scan_sql_error)?;
        if hidden != 0 {
            names.push(format!("__hidden_{}", names.len()));
            continue;
        }
        names.push(name);
    }

    if names.len() != LEGACY_COLUMN_COUNT {
        let projection_safe = LEGACY_COLUMNS
            .iter()
            .all(|column| names.iter().any(|name| name == column));
        return Ok(LegacySchemaStatus::Unavailable {
            reason: LegacyJournalDedupIdentityUnavailable::Schema {
                reason: String::from("legacy journal schema is missing or has extra columns"),
            },
            projection_safe,
        });
    }
    names.sort_unstable();
    let mut expected = LEGACY_COLUMNS.to_vec();
    expected.sort_unstable();
    if names != expected {
        let projection_safe = LEGACY_COLUMNS
            .iter()
            .all(|column| names.iter().any(|name| name == column));
        return Ok(LegacySchemaStatus::Unavailable {
            reason: LegacyJournalDedupIdentityUnavailable::Schema {
                reason: String::from("legacy journal schema column set is not canonical"),
            },
            projection_safe,
        });
    }
    Ok(LegacySchemaStatus::Complete)
}

fn unavailable_for_schema_or_row(
    schema: &LegacySchemaStatus,
    row: LegacyJournalDedupIdentityUnavailable,
) -> LegacyJournalDedupIdentityResult {
    match schema {
        LegacySchemaStatus::Complete => LegacyJournalDedupIdentityResult::Unavailable(row),
        LegacySchemaStatus::Unavailable { reason, .. } => {
            LegacyJournalDedupIdentityResult::Unavailable(reason.clone())
        }
    }
}

fn load_candidates(
    connection: &Connection,
    upper_rowid: i64,
    after_rowid: Option<i64>,
) -> Result<Vec<LegacyCandidate>, LegacyJournalScanError> {
    let metadata = LEGACY_COLUMNS
        .iter()
        .map(|column| {
            format!(
                "typeof({column}), length(CAST({column} AS BLOB)), CASE WHEN typeof({column}) IN ('text', 'blob') THEN substr(CAST({column} AS BLOB), 1, {MAX_LEGACY_JOURNAL_PREVIEW_BYTES}) ELSE NULL END"
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let statement = match after_rowid {
        Some(_) => format!(
            "SELECT rowid, {metadata}
             FROM file_ops_journal
             WHERE rowid > ?1 AND rowid <= ?2
             ORDER BY rowid ASC
             LIMIT ?3"
        ),
        None => format!(
            "SELECT rowid, {metadata}
             FROM file_ops_journal
             WHERE rowid <= ?1
             ORDER BY rowid ASC
             LIMIT ?2"
        ),
    };
    let mut statement = connection
        .prepare(&statement)
        .map_err(SourceDbError::from)?;
    let mut rows = match after_rowid {
        Some(after_rowid) => statement
            .query(params![after_rowid, upper_rowid, PAGE_QUERY_LIMIT])
            .map_err(SourceDbError::from)?,
        None => statement
            .query(params![upper_rowid, PAGE_QUERY_LIMIT])
            .map_err(SourceDbError::from)?,
    };
    let mut candidates = Vec::with_capacity(MAX_LEGACY_JOURNAL_ROWS_PER_PAGE + 1);
    while let Some(row) = rows.next().map_err(SourceDbError::from)? {
        candidates.push(LegacyCandidate::read(row)?);
    }
    Ok(candidates)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LegacyFieldObservation {
    sql_type: String,
    length: Option<i64>,
    preview: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
struct LegacyCandidate {
    rowid: i64,
    fields: Vec<LegacyFieldObservation>,
}

impl LegacyCandidate {
    fn read(row: &rusqlite::Row<'_>) -> Result<Self, LegacyJournalScanError> {
        let rowid = row.get(0).map_err(scan_sql_error)?;
        let mut fields = Vec::with_capacity(LEGACY_COLUMN_COUNT);
        for index in 0..LEGACY_COLUMN_COUNT {
            let base = 1 + index * 3;
            let sql_type = row.get(base).map_err(scan_sql_error)?;
            let length = row.get(base + 1).map_err(scan_sql_error)?;
            let preview = match row.get_ref(base + 2).map_err(scan_sql_error)? {
                rusqlite::types::ValueRef::Null => None,
                rusqlite::types::ValueRef::Blob(bytes) => Some(bytes.to_vec()),
                _ => {
                    return Err(LegacyJournalScanError::RequiresAudit {
                        reason: String::from("legacy journal preview returned an unexpected type"),
                    });
                }
            };
            fields.push(LegacyFieldObservation {
                sql_type,
                length,
                preview,
            });
        }
        Ok(Self { rowid, fields })
    }

    fn key(&self) -> LegacyJournalKey {
        let field = &self.fields[0];
        if field.sql_type != "text" {
            return LegacyJournalKey::Invalid {
                sql_type: field.sql_type.clone(),
                preview: preview_text(field.preview.as_deref()),
            };
        }
        let preview = preview_text(field.preview.as_deref());
        match field.length {
            Some(length) if length >= 0 && usize::try_from(length).ok() == Some(preview.len()) => {
                LegacyJournalKey::Text(preview)
            }
            Some(_) => LegacyJournalKey::TextPreview(preview),
            None => LegacyJournalKey::Invalid {
                sql_type: String::from("text"),
                preview: String::from("id text length is NULL"),
            },
        }
    }

    fn metadata_issue(&self) -> Option<LegacyJournalRetentionReason> {
        for (index, field) in self.fields.iter().enumerate() {
            let descriptor = LegacyJournalField::from_index(index);
            if descriptor.is_text() {
                if field.sql_type == "null" && descriptor.allows_null() {
                    continue;
                }
                if field.sql_type != "text" {
                    return Some(LegacyJournalRetentionReason::TypeInvalid {
                        field: descriptor,
                        sql_type: field.sql_type.clone(),
                        preview: preview_text(field.preview.as_deref()),
                    });
                }
                let Some(length) = field.length else {
                    return Some(LegacyJournalRetentionReason::TypeInvalid {
                        field: descriptor,
                        sql_type: String::from("text"),
                        preview: String::from("text length is NULL"),
                    });
                };
                if length < 0
                    || usize::try_from(length)
                        .map_or(true, |value| value > MAX_LEGACY_JOURNAL_FIELD_BYTES)
                {
                    return Some(LegacyJournalRetentionReason::SizeInvalid {
                        field: descriptor,
                        declared: Some(length),
                        reason: String::from("legacy text field exceeds the bounded field size"),
                    });
                }
            } else {
                if field.sql_type == "null" && descriptor.allows_null() {
                    continue;
                }
                if field.sql_type != "integer" {
                    return Some(LegacyJournalRetentionReason::TypeInvalid {
                        field: descriptor,
                        sql_type: field.sql_type.clone(),
                        preview: preview_text(field.preview.as_deref()),
                    });
                }
            }
        }
        None
    }

    fn text_bytes(&self) -> Result<usize, LegacyJournalScanError> {
        self.fields
            .iter()
            .enumerate()
            .filter(|(index, _)| LegacyJournalField::from_index(*index).is_text())
            .try_fold(0_usize, |total, (_, field)| {
                let Some(length) = field.length else {
                    return Ok(total);
                };
                let length =
                    usize::try_from(length).map_err(|_| LegacyJournalScanError::RequiresAudit {
                        reason: String::from("legacy text length cannot be represented safely"),
                    })?;
                total
                    .checked_add(length)
                    .ok_or_else(|| LegacyJournalScanError::RequiresAudit {
                        reason: String::from(
                            "legacy row byte length overflowed the bounded reader",
                        ),
                    })
            })
    }

    fn fingerprint_bytes(&self) -> Result<usize, LegacyJournalDedupIdentityUnavailable> {
        let mut bytes = LEGACY_DEDUP_FINGERPRINT_DOMAIN
            .len()
            .saturating_add(std::mem::size_of::<u8>());
        for (index, field) in self.fields.iter().enumerate() {
            let descriptor = LegacyJournalField::from_index(index);
            let name = LEGACY_COLUMNS[index].as_bytes();
            bytes = bytes
                .checked_add(std::mem::size_of::<u32>())
                .and_then(|total| total.checked_add(name.len()))
                .and_then(|total| total.checked_add(std::mem::size_of::<u8>()))
                .and_then(|total| total.checked_add(std::mem::size_of::<u64>()))
                .ok_or_else(|| LegacyJournalDedupIdentityUnavailable::ValueUnavailable {
                    field: descriptor,
                    reason: String::from("legacy fingerprint byte count overflowed"),
                })?;
            let value_bytes = match field.sql_type.as_str() {
                "null" => 0,
                "integer" | "real" => std::mem::size_of::<i64>(),
                "text" | "blob" => {
                    let Some(length) = field.length else {
                        return Err(LegacyJournalDedupIdentityUnavailable::ValueUnavailable {
                            field: descriptor,
                            reason: String::from("raw value length is unavailable"),
                        });
                    };
                    if length < 0
                        || usize::try_from(length)
                            .map_or(true, |value| value > MAX_LEGACY_JOURNAL_FIELD_BYTES)
                    {
                        return Err(LegacyJournalDedupIdentityUnavailable::ValueTooLarge {
                            field: descriptor,
                            declared: Some(length),
                        });
                    }
                    usize::try_from(length).map_err(|_| {
                        LegacyJournalDedupIdentityUnavailable::ValueUnavailable {
                            field: descriptor,
                            reason: String::from("raw value length cannot be represented safely"),
                        }
                    })?
                }
                sql_type => {
                    return Err(LegacyJournalDedupIdentityUnavailable::ValueUnavailable {
                        field: descriptor,
                        reason: format!("unsupported SQLite storage class `{sql_type}`"),
                    });
                }
            };
            bytes = bytes.checked_add(value_bytes).ok_or_else(|| {
                LegacyJournalDedupIdentityUnavailable::ValueUnavailable {
                    field: descriptor,
                    reason: String::from("legacy fingerprint byte count overflowed"),
                }
            })?;
        }
        Ok(bytes)
    }
}

#[derive(Debug)]
enum BoundedRow {
    Missing {
        identity: LegacyJournalDedupIdentityResult,
    },
    Changed {
        reason: String,
        identity: LegacyJournalDedupIdentityResult,
    },
    Decoded {
        entry: FileOpJournalEntry,
        identity: LegacyJournalDedupIdentityResult,
        bytes: usize,
    },
    Malformed {
        malformed: MalformedJournalEntry,
        identity: LegacyJournalDedupIdentityResult,
        bytes: usize,
    },
    Retained {
        reason: LegacyJournalRetentionReason,
        identity: LegacyJournalDedupIdentityResult,
        bytes: usize,
    },
}

fn load_bounded_row(
    connection: &Connection,
    candidate: &LegacyCandidate,
    source_binding: &(String, String),
    schema: &LegacySchemaStatus,
) -> Result<BoundedRow, LegacyJournalScanError> {
    let values = LEGACY_COLUMNS
        .iter()
        .map(|column| {
            format!(
                "CASE WHEN typeof({column}) IN ('null', 'integer', 'real')
                     OR (typeof({column}) IN ('text', 'blob')
                         AND length(CAST({column} AS BLOB)) <= {MAX_LEGACY_JOURNAL_FIELD_BYTES})
                     THEN {column} ELSE NULL END"
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let metadata = LEGACY_COLUMNS
        .iter()
        .map(|column| format!("typeof({column}), length(CAST({column} AS BLOB))"))
        .collect::<Vec<_>>()
        .join(", ");
    let statement = format!(
        "SELECT {values}, {metadata}
         FROM file_ops_journal
         WHERE rowid = ?1"
    );
    let mut statement = connection
        .prepare(&statement)
        .map_err(SourceDbError::from)?;
    let mut rows = statement
        .query(params![candidate.rowid])
        .map_err(SourceDbError::from)?;
    let Some(row) = rows.next().map_err(scan_sql_error)? else {
        return Ok(BoundedRow::Missing {
            identity: unavailable_for_schema_or_row(
                schema,
                LegacyJournalDedupIdentityUnavailable::RowDisappeared,
            ),
        });
    };
    let mut current_fields = Vec::with_capacity(LEGACY_COLUMN_COUNT);
    for index in 0..LEGACY_COLUMN_COUNT {
        let base = LEGACY_COLUMN_COUNT + index * 2;
        let sql_type: String = row.get(base).map_err(scan_sql_error)?;
        let length = row.get(base + 1).map_err(scan_sql_error)?;
        current_fields.push(LegacyFieldObservation {
            sql_type,
            length,
            preview: None,
        });
    }
    if current_fields
        .iter()
        .zip(candidate.fields.iter())
        .any(|(current, candidate)| {
            current.sql_type != candidate.sql_type || current.length != candidate.length
        })
    {
        return Ok(BoundedRow::Changed {
            reason: String::from("legacy row metadata changed during bounded read"),
            identity: unavailable_for_schema_or_row(
                schema,
                LegacyJournalDedupIdentityUnavailable::RowChanged {
                    reason: String::from("legacy row metadata changed during bounded read"),
                },
            ),
        });
    }

    let bytes = match candidate.fingerprint_bytes() {
        Ok(bytes) => bytes,
        Err(unavailable) => {
            let identity = unavailable_for_schema_or_row(schema, unavailable);
            let reason =
                candidate
                    .metadata_issue()
                    .unwrap_or(LegacyJournalRetentionReason::Malformed {
                        id: match candidate.key() {
                            LegacyJournalKey::Text(value)
                            | LegacyJournalKey::TextPreview(value) => Some(value),
                            LegacyJournalKey::Invalid { .. } => None,
                        },
                        detail: String::from("legacy raw value could not be fingerprinted"),
                    });
            return Ok(BoundedRow::Retained {
                reason,
                identity,
                bytes: 0,
            });
        }
    };
    let identity = if matches!(schema, LegacySchemaStatus::Complete) {
        match fingerprint_row(row, candidate.rowid, source_binding) {
            Ok(identity) => LegacyJournalDedupIdentityResult::Complete(identity),
            Err(unavailable) => {
                let reason =
                    candidate
                        .metadata_issue()
                        .unwrap_or(LegacyJournalRetentionReason::Malformed {
                            id: match candidate.key() {
                                LegacyJournalKey::Text(value)
                                | LegacyJournalKey::TextPreview(value) => Some(value),
                                LegacyJournalKey::Invalid { .. } => None,
                            },
                            detail: String::from("legacy raw value could not be fingerprinted"),
                        });
                return Ok(BoundedRow::Retained {
                    reason,
                    identity: LegacyJournalDedupIdentityResult::Unavailable(unavailable),
                    bytes: 0,
                });
            }
        }
    } else {
        unavailable_for_schema_or_row(
            schema,
            LegacyJournalDedupIdentityUnavailable::ValueUnavailable {
                field: LegacyJournalField::Row,
                reason: String::from("legacy journal schema is not canonical"),
            },
        )
    };
    if let Some(reason) = candidate.metadata_issue() {
        return Ok(BoundedRow::Retained {
            reason,
            identity,
            bytes,
        });
    }
    match decode_journal_row(row) {
        Ok(entry) => Ok(BoundedRow::Decoded {
            entry,
            identity,
            bytes,
        }),
        Err(malformed) => Ok(BoundedRow::Malformed {
            malformed,
            identity,
            bytes,
        }),
    }
}

fn fingerprint_row(
    row: &rusqlite::Row<'_>,
    rowid: i64,
    source_binding: &(String, String),
) -> Result<LegacyJournalDedupIdentity, LegacyJournalDedupIdentityUnavailable> {
    let mut hasher = Hasher::new();
    hasher.update(LEGACY_DEDUP_FINGERPRINT_DOMAIN);
    hasher.update(&[LEGACY_DEDUP_FINGERPRINT_VERSION]);
    for (index, column) in LEGACY_COLUMNS.iter().enumerate() {
        let descriptor = LegacyJournalField::from_index(index);
        let name = column.as_bytes();
        let name_len = u32::try_from(name.len()).map_err(|_| {
            LegacyJournalDedupIdentityUnavailable::ValueUnavailable {
                field: descriptor,
                reason: String::from("legacy fingerprint column name is too long"),
            }
        })?;
        hasher.update(&name_len.to_le_bytes());
        hasher.update(name);
        let value = row.get_ref(index).map_err(|_| {
            LegacyJournalDedupIdentityUnavailable::ValueUnavailable {
                field: descriptor,
                reason: String::from("legacy raw value could not be read"),
            }
        })?;
        let tag = match value {
            rusqlite::types::ValueRef::Null => 0_u8,
            rusqlite::types::ValueRef::Integer(_) => 1_u8,
            rusqlite::types::ValueRef::Real(_) => 2_u8,
            rusqlite::types::ValueRef::Text(_) => 3_u8,
            rusqlite::types::ValueRef::Blob(_) => 4_u8,
        };
        hasher.update(&[tag]);
        match value {
            rusqlite::types::ValueRef::Null => {
                hasher.update(&0_u64.to_le_bytes());
            }
            rusqlite::types::ValueRef::Integer(value) => {
                hasher.update(&8_u64.to_le_bytes());
                hasher.update(&value.to_le_bytes());
            }
            rusqlite::types::ValueRef::Real(value) => {
                hasher.update(&8_u64.to_le_bytes());
                hasher.update(&value.to_bits().to_le_bytes());
            }
            rusqlite::types::ValueRef::Text(value) | rusqlite::types::ValueRef::Blob(value) => {
                if value.len() > MAX_LEGACY_JOURNAL_FIELD_BYTES {
                    return Err(LegacyJournalDedupIdentityUnavailable::ValueTooLarge {
                        field: descriptor,
                        declared: i64::try_from(value.len()).ok(),
                    });
                }
                let length = u64::try_from(value.len()).map_err(|_| {
                    LegacyJournalDedupIdentityUnavailable::ValueUnavailable {
                        field: descriptor,
                        reason: String::from(
                            "legacy raw value length cannot be represented safely",
                        ),
                    }
                })?;
                hasher.update(&length.to_le_bytes());
                hasher.update(value);
            }
        }
    }
    Ok(LegacyJournalDedupIdentity {
        root_identity: source_binding.0.clone(),
        database_identity: source_binding.1.clone(),
        rowid,
        fingerprint_version: LEGACY_DEDUP_FINGERPRINT_VERSION,
        fingerprint: *hasher.finalize().as_bytes(),
    })
}

fn map_malformed(malformed: MalformedJournalEntry) -> LegacyJournalRetentionReason {
    let id = malformed.id.map(|id| bounded_text(&id));
    let raw_detail = malformed.detail;
    let detail = bounded_text(&raw_detail);
    if let Some(value) = raw_detail
        .strip_prefix("unknown op_type value `")
        .and_then(|value| value.strip_suffix('`'))
    {
        return LegacyJournalRetentionReason::UnknownOperationType {
            id,
            value: bounded_text(value),
        };
    }
    if let Some(value) = raw_detail
        .strip_prefix("unknown stage value `")
        .and_then(|value| value.strip_suffix('`'))
    {
        return LegacyJournalRetentionReason::UnknownStage {
            id,
            value: bounded_text(value),
        };
    }
    LegacyJournalRetentionReason::Malformed { id, detail }
}

fn preview_text(bytes: Option<&[u8]>) -> String {
    let Some(bytes) = bytes else {
        return String::new();
    };
    bounded_text(&String::from_utf8_lossy(bytes))
}

fn bounded_text(value: &str) -> String {
    let mut end = 0;
    for (index, character) in value.char_indices() {
        let next = index + character.len_utf8();
        if next > MAX_LEGACY_JOURNAL_PREVIEW_BYTES {
            break;
        }
        end = next;
    }
    value[..end].to_owned()
}

fn scan_sql_error(error: rusqlite::Error) -> LegacyJournalScanError {
    LegacyJournalScanError::SourceDb(SourceDbError::from(error))
}

impl From<rusqlite::Error> for LegacyJournalScanError {
    fn from(error: rusqlite::Error) -> Self {
        scan_sql_error(error)
    }
}

impl Display for LegacyJournalField {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_dirs::{AppRootGuard, WritableProfileGuard};
    use crate::sample_sources::SourceDatabase;
    use crate::test_runtime::TestRuntimeGuard;
    use rusqlite::{Connection, params};
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::time::Duration;
    use tempfile::{TempDir, tempdir};

    struct ReaderFixture {
        db: SourceDatabase,
        _source_root: TempDir,
        _database_root: TempDir,
        _app_root: TempDir,
        _app_root_guard: AppRootGuard,
        _profile: WritableProfileGuard,
        _runtime: TestRuntimeGuard,
    }

    impl ReaderFixture {
        fn new() -> Self {
            let runtime = TestRuntimeGuard::acquire();
            let app_root = tempdir().expect("app root");
            let app_root_guard = AppRootGuard::set(
                std::fs::canonicalize(app_root.path()).expect("canonical app root"),
            )
            .expect("app root override");
            let profile = WritableProfileGuard::acquire_current().expect("profile owner");
            let source_root = tempdir().expect("source root");
            let database_root = tempdir().expect("database root");
            let db = SourceDatabase::open_for_source_write_with_profile_guard(
                source_root.path(),
                &profile,
                database_root.path(),
            )
            .expect("owned source database");
            Self {
                db,
                _source_root: source_root,
                _database_root: database_root,
                _app_root: app_root,
                _app_root_guard: app_root_guard,
                _profile: profile,
                _runtime: runtime,
            }
        }
    }

    fn insert_copy(
        db: &SourceDatabase,
        rowid: Option<i64>,
        id: &str,
        stage: &str,
        target_relative: &str,
    ) {
        match rowid {
            Some(rowid) => db
                .connection
                .execute(
                    "INSERT INTO file_ops_journal
                         (rowid, id, op_type, stage, target_relative, created_at)
                     VALUES (?1, ?2, 'copy', ?3, ?4, 1)",
                    params![rowid, id, stage, target_relative],
                )
                .expect("insert copy row"),
            None => db
                .connection
                .execute(
                    "INSERT INTO file_ops_journal
                         (id, op_type, stage, target_relative, created_at)
                     VALUES (?1, 'copy', ?2, ?3, 1)",
                    params![id, stage, target_relative],
                )
                .expect("insert copy row"),
        };
    }

    fn insert_move(db: &SourceDatabase, id: &str, stage: &str) {
        db.connection
            .execute(
                "INSERT INTO file_ops_journal
                     (id, op_type, stage, source_root, source_relative, target_relative,
                      staged_relative, created_at)
                 VALUES (?1, 'move', ?2, '/source', 'input.wav', 'target.wav', 'staged.wav', 1)",
                params![id, stage],
            )
            .expect("insert move row");
    }

    #[test]
    fn owned_reader_decodes_copy_move_and_recovery_stages() {
        let fixture = ReaderFixture::new();
        insert_copy(&fixture.db, None, "copy-intent", "intent", "copy.wav");
        insert_copy(&fixture.db, None, "copy-target", "target_db", "target.wav");
        insert_move(&fixture.db, "move-staged", "staged");
        insert_move(&fixture.db, "move-source", "source_db");

        let page = scan_page(&fixture.db, None).expect("bounded page");

        assert!(page.is_done());
        assert_eq!(page.entries().len(), 4);
        let ids = page
            .entries()
            .iter()
            .map(|entry| match entry.outcome() {
                LegacyJournalRowOutcome::Decoded(entry) => entry.id.as_str(),
                LegacyJournalRowOutcome::Retained(reason) => {
                    panic!("healthy row was retained: {reason:?}")
                }
            })
            .collect::<HashSet<_>>();
        assert_eq!(
            ids,
            HashSet::from(["copy-intent", "copy-target", "move-staged", "move-source",])
        );
    }

    #[test]
    fn dedup_identity_is_complete_stable_and_source_bound() {
        let fixture = ReaderFixture::new();
        insert_copy(&fixture.db, None, "identity-row", "intent", "identity.wav");

        let first = scan_page(&fixture.db, None).expect("first identity page");
        let first_identity = first.entries()[0].dedup_identity().clone();
        let second = scan_page(&fixture.db, None).expect("second identity page");
        assert_eq!(second.entries()[0].dedup_identity(), &first_identity);
        let LegacyJournalDedupIdentityResult::Complete(identity) = first_identity else {
            panic!("bounded canonical legacy row should have a complete identity");
        };
        assert_eq!(identity.rowid(), first.entries()[0].rowid());
        assert_eq!(
            identity.fingerprint_version(),
            LEGACY_DEDUP_FINGERPRINT_VERSION
        );
        assert!(!identity.root_identity().is_empty());
        assert!(!identity.database_identity().is_empty());
        assert_ne!(identity.fingerprint(), &[0; 32]);
    }

    #[test]
    fn dedup_identity_survives_reopen_of_the_same_bound_database() {
        let runtime = TestRuntimeGuard::acquire();
        let app_root = tempdir().expect("app root");
        let app_root_guard =
            AppRootGuard::set(std::fs::canonicalize(app_root.path()).expect("canonical app root"))
                .expect("app root override");
        let profile = WritableProfileGuard::acquire_current().expect("profile owner");
        let source_root = tempdir().expect("source root");
        let database_root = tempdir().expect("database root");
        let db = SourceDatabase::open_for_source_write_with_profile_guard(
            source_root.path(),
            &profile,
            database_root.path(),
        )
        .expect("owned source database");
        insert_copy(&db, None, "reopen-identity", "intent", "reopen.wav");
        let before = scan_page(&db, None).expect("pre-close identity page");
        let before_identity = before.entries()[0].dedup_identity().clone();
        drop(db);

        let reopened = SourceDatabase::open_for_source_write_with_profile_guard(
            source_root.path(),
            &profile,
            database_root.path(),
        )
        .expect("reopened source database");
        let after = scan_page(&reopened, None).expect("post-reopen identity page");
        assert_eq!(after.entries()[0].dedup_identity(), &before_identity);
        drop(reopened);
        drop(profile);
        drop(app_root_guard);
        drop(app_root);
        drop(source_root);
        drop(database_root);
        drop(runtime);
    }

    #[test]
    fn dedup_fingerprint_covers_raw_values_beyond_previews_and_sqlite_types() {
        let fixture = ReaderFixture::new();
        let long_a = format!("{}a", "x".repeat(MAX_LEGACY_JOURNAL_PREVIEW_BYTES + 64));
        let long_b = format!("{}b", "x".repeat(MAX_LEGACY_JOURNAL_PREVIEW_BYTES + 64));
        insert_copy(&fixture.db, None, "raw-long", "intent", &long_a);
        fixture
            .db
            .connection
            .execute(
                "INSERT INTO file_ops_journal
                    (id, op_type, stage, target_relative, file_size, created_at)
                 VALUES ('raw-type', 'copy', 'intent', 'type.wav', 1, 1)",
                [],
            )
            .expect("raw type rows");
        let before = scan_page(&fixture.db, None).expect("raw fingerprint page");
        let before_long = before
            .entries()
            .iter()
            .find(
                |entry| matches!(entry.key(), LegacyJournalKey::Text(value) if value == "raw-long"),
            )
            .expect("long row")
            .dedup_identity()
            .clone();
        let mut type_fingerprints = vec![
            before
                .entries()
                .iter()
                .find(|entry| {
                    matches!(entry.key(), LegacyJournalKey::Text(value) if value == "raw-type")
                })
                .expect("raw type row")
                .dedup_identity()
                .clone(),
        ];
        for raw_value in ["1.5", "'text'", "zeroblob(1)", "NULL"] {
            fixture
                .db
                .connection
                .execute(
                    &format!(
                        "UPDATE file_ops_journal SET file_size = {raw_value} WHERE id = 'raw-type'"
                    ),
                    [],
                )
                .expect("change raw SQLite type");
            let page = scan_page(&fixture.db, None).expect("raw type fingerprint page");
            type_fingerprints.push(
                page.entries()
                    .iter()
                    .find(|entry| {
                        matches!(entry.key(), LegacyJournalKey::Text(value) if value == "raw-type")
                    })
                    .expect("raw type row")
                    .dedup_identity()
                    .clone(),
            );
        }
        assert!(
            type_fingerprints
                .iter()
                .all(|identity| matches!(identity, LegacyJournalDedupIdentityResult::Complete(_)))
        );
        for left in 0..type_fingerprints.len() {
            for right in (left + 1)..type_fingerprints.len() {
                assert_ne!(type_fingerprints[left], type_fingerprints[right]);
            }
        }

        fixture
            .db
            .connection
            .execute(
                "UPDATE file_ops_journal SET target_relative = ?1 WHERE id = 'raw-long'",
                params![long_b],
            )
            .expect("change raw tail");
        let after = scan_page(&fixture.db, None).expect("changed raw fingerprint page");
        let after_long = after
            .entries()
            .iter()
            .find(
                |entry| matches!(entry.key(), LegacyJournalKey::Text(value) if value == "raw-long"),
            )
            .expect("changed long row")
            .dedup_identity();
        assert_ne!(&before_long, after_long);
    }

    #[test]
    fn malformed_and_extra_schema_rows_retain_bounded_identity_evidence() {
        let fixture = ReaderFixture::new();
        fixture
            .db
            .connection
            .execute(
                "INSERT INTO file_ops_journal
                    (id, op_type, stage, target_relative, file_size, created_at)
                 VALUES ('malformed-identity', 'copy', 'intent', 'bad.wav', 'bad', 1)",
                [],
            )
            .expect("malformed row");
        let page = scan_page(&fixture.db, None).expect("malformed identity page");
        assert!(matches!(
            page.entries()[0].outcome(),
            LegacyJournalRowOutcome::Retained(LegacyJournalRetentionReason::TypeInvalid { .. })
        ));
        assert!(matches!(
            page.entries()[0].dedup_identity(),
            LegacyJournalDedupIdentityResult::Complete(_)
        ));

        fixture
            .db
            .connection
            .execute(
                "ALTER TABLE file_ops_journal ADD COLUMN future_extra TEXT",
                [],
            )
            .expect("extra schema column");
        let extra_page = scan_page(&fixture.db, None).expect("extra schema page");
        assert!(extra_page.entries().iter().all(|entry| matches!(
            entry.dedup_identity(),
            LegacyJournalDedupIdentityResult::Unavailable(
                LegacyJournalDedupIdentityUnavailable::Schema { .. }
            )
        )));
    }

    #[test]
    fn oversized_raw_value_has_unavailable_identity_without_partial_digest() {
        let fixture = ReaderFixture::new();
        let oversized = "x".repeat(MAX_LEGACY_JOURNAL_FIELD_BYTES + 1);
        insert_copy(
            &fixture.db,
            None,
            "oversized-identity",
            "intent",
            &oversized,
        );
        let page = scan_page(&fixture.db, None).expect("oversized identity page");
        assert!(matches!(
            page.entries()[0].dedup_identity(),
            LegacyJournalDedupIdentityResult::Unavailable(
                LegacyJournalDedupIdentityUnavailable::ValueTooLarge { .. }
            )
        ));
    }

    #[test]
    fn reader_retains_malformed_rows_and_discovers_later_rows() {
        let fixture = ReaderFixture::new();
        insert_copy(&fixture.db, None, "unknown-kind", "intent", "kind.wav");
        fixture
            .db
            .connection
            .execute(
                "UPDATE file_ops_journal SET op_type = 'future' WHERE id = 'unknown-kind'",
                [],
            )
            .expect("unknown kind");
        insert_copy(
            &fixture.db,
            None,
            "unknown-stage",
            "future_stage",
            "stage.wav",
        );
        fixture
            .db
            .connection
            .execute(
                "INSERT INTO file_ops_journal
                     (id, op_type, stage, target_relative, file_size, created_at)
                 VALUES ('bad-type', 'copy', 'intent', 'type.wav', 'not-an-integer', 1)",
                [],
            )
            .expect("invalid SQL type");
        fixture
            .db
            .connection
            .execute(
                "INSERT INTO file_ops_journal
                     (id, op_type, stage, target_relative, created_at)
                 VALUES ('bad-path', 'copy', 'intent', '/absolute.wav', 1)",
                [],
            )
            .expect("invalid path");
        fixture
            .db
            .connection
            .execute(
                "INSERT INTO file_ops_journal
                     (id, op_type, stage, target_relative, created_at)
                 VALUES ('bad-target-type', 'copy', 'intent', zeroblob(4), 1)",
                [],
            )
            .expect("blob target");
        insert_copy(
            &fixture.db,
            None,
            "healthy-after-errors",
            "intent",
            "later.wav",
        );

        let page = scan_page(&fixture.db, None).expect("bounded malformed page");
        assert_eq!(page.entries().len(), 6);
        assert!(page.entries().iter().any(|entry| {
            matches!(
                entry.outcome(),
                LegacyJournalRowOutcome::Retained(
                    LegacyJournalRetentionReason::UnknownOperationType { .. }
                )
            )
        }));
        assert!(page.entries().iter().any(|entry| {
            matches!(
                entry.outcome(),
                LegacyJournalRowOutcome::Retained(
                    LegacyJournalRetentionReason::UnknownStage { .. }
                )
            )
        }));
        assert!(page.entries().iter().any(|entry| {
            matches!(
                entry.outcome(),
                LegacyJournalRowOutcome::Retained(LegacyJournalRetentionReason::TypeInvalid {
                    field: LegacyJournalField::FileSize,
                    ..
                })
            )
        }));
        assert!(page.entries().iter().any(|entry| {
            matches!(
                entry.outcome(),
                LegacyJournalRowOutcome::Retained(LegacyJournalRetentionReason::Malformed {
                    id: Some(id),
                    ..
                }) if id == "bad-path"
            )
        }));
        assert!(page.entries().iter().any(|entry| {
            matches!(
                entry.outcome(),
                LegacyJournalRowOutcome::Retained(LegacyJournalRetentionReason::TypeInvalid {
                    field: LegacyJournalField::TargetRelative,
                    ..
                })
            )
        }));
        assert!(page.entries().iter().any(|entry| {
            matches!(
                entry.outcome(),
                LegacyJournalRowOutcome::Decoded(decoded) if decoded.id == "healthy-after-errors"
            )
        }));
    }

    #[test]
    fn oversized_early_row_is_retained_without_hiding_later_rows() {
        let fixture = ReaderFixture::new();
        let oversized = "x".repeat(MAX_LEGACY_JOURNAL_FIELD_BYTES + 1);
        insert_copy(&fixture.db, None, "oversized", "intent", oversized.as_str());
        insert_copy(&fixture.db, None, "after-oversized", "intent", "after.wav");

        let page = scan_page(&fixture.db, None).expect("bounded oversized page");
        assert_eq!(page.entries().len(), 2);
        assert!(matches!(
            page.entries()[0].outcome(),
            LegacyJournalRowOutcome::Retained(LegacyJournalRetentionReason::SizeInvalid {
                field: LegacyJournalField::TargetRelative,
                ..
            })
        ));
        assert!(matches!(
            page.entries()[1].outcome(),
            LegacyJournalRowOutcome::Decoded(decoded) if decoded.id == "after-oversized"
        ));
    }

    #[test]
    fn reader_pages_are_bounded_and_cursor_covers_all_rows() {
        let fixture = ReaderFixture::new();
        let transaction = fixture
            .db
            .connection
            .unchecked_transaction()
            .expect("bulk insert transaction");
        for index in 0..4096 {
            transaction
                .execute(
                    "INSERT INTO file_ops_journal
                         (id, op_type, stage, target_relative, created_at)
                     VALUES (?1, 'copy', 'intent', ?2, 1)",
                    params![format!("row-{index}"), format!("row-{index}.wav")],
                )
                .expect("bulk insert row");
        }
        transaction.commit().expect("bulk insert commit");

        let mut cursor = None;
        let mut rowids = Vec::with_capacity(4096);
        let mut pages = 0;
        loop {
            let page = scan_page(&fixture.db, cursor.take()).expect("bounded row page");
            assert!(page.entries().len() <= MAX_LEGACY_JOURNAL_ROWS_PER_PAGE);
            assert!(page.bytes() <= MAX_LEGACY_JOURNAL_BYTES_PER_PAGE);
            rowids.extend(page.entries().iter().map(LegacyJournalEntry::rowid));
            pages += 1;
            if page.is_done() {
                assert!(page.next_cursor().is_none());
                break;
            }
            cursor = page.next_cursor();
            assert!(cursor.is_some());
        }

        assert_eq!(rowids.len(), 4096);
        assert!(rowids.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(pages, 256);
    }

    #[test]
    fn cursor_does_not_include_rows_appended_after_first_page() {
        let fixture = ReaderFixture::new();
        for index in 0..17 {
            insert_copy(
                &fixture.db,
                None,
                &format!("before-{index}"),
                "intent",
                &format!("before-{index}.wav"),
            );
        }
        let first_page = scan_page(&fixture.db, None).expect("first page");
        let upper_rowid = first_page
            .next_cursor()
            .expect("second page cursor")
            .upper_rowid();
        insert_copy(&fixture.db, None, "appended", "intent", "appended.wav");

        let mut cursor = first_page.next_cursor();
        let mut ids = Vec::new();
        while let Some(current) = cursor {
            let page = scan_page(&fixture.db, Some(current)).expect("captured page");
            ids.extend(
                page.entries()
                    .iter()
                    .filter_map(|entry| match entry.outcome() {
                        LegacyJournalRowOutcome::Decoded(decoded) => Some(decoded.id.clone()),
                        LegacyJournalRowOutcome::Retained(_) => None,
                    }),
            );
            cursor = page.next_cursor();
            if page.is_done() {
                break;
            }
        }
        assert!(ids.iter().all(|id| id != "appended"));
        assert!(ids.iter().all(|id| id.starts_with("before-")));
        assert_eq!(ids.len(), 1);
        assert_eq!(upper_rowid, 17);

        let mut fresh_cursor = None;
        let mut found_appended = false;
        loop {
            let page = scan_page(&fixture.db, fresh_cursor.take()).expect("fresh page");
            found_appended |= page.entries().iter().any(|entry| {
                matches!(
                    entry.outcome(),
                    LegacyJournalRowOutcome::Decoded(decoded) if decoded.id == "appended"
                )
            });
            if page.is_done() {
                break;
            }
            fresh_cursor = page.next_cursor();
        }
        assert!(found_appended);
    }

    #[test]
    fn signed_rowids_are_observed_through_i64_max() {
        let fixture = ReaderFixture::new();
        insert_copy(&fixture.db, Some(-1), "negative", "intent", "negative.wav");
        insert_copy(&fixture.db, Some(0), "zero", "intent", "zero.wav");
        insert_copy(
            &fixture.db,
            Some(i64::MAX),
            "maximum",
            "intent",
            "maximum.wav",
        );

        let page = scan_page(&fixture.db, None).expect("signed rowid page");
        assert!(page.is_done());
        assert_eq!(
            page.entries()
                .iter()
                .map(LegacyJournalEntry::rowid)
                .collect::<Vec<_>>(),
            [-1, 0, i64::MAX]
        );
    }

    #[test]
    fn cursor_is_weak_and_reopen_invalidates_it() {
        let runtime = TestRuntimeGuard::acquire();
        let app_root = tempdir().expect("app root");
        let app_root_guard =
            AppRootGuard::set(std::fs::canonicalize(app_root.path()).expect("canonical app root"))
                .expect("app root override");
        let profile = WritableProfileGuard::acquire_current().expect("profile owner");
        let source_root = tempdir().expect("source root");
        let database_root = tempdir().expect("database root");
        let db = SourceDatabase::open_for_source_write_with_profile_guard(
            source_root.path(),
            &profile,
            database_root.path(),
        )
        .expect("owned source database");
        for index in 0..17 {
            insert_copy(
                &db,
                None,
                &format!("cursor-{index}"),
                "intent",
                &format!("cursor-{index}.wav"),
            );
        }
        let cursor = scan_page(&db, None)
            .expect("first cursor page")
            .next_cursor()
            .expect("second cursor page");
        let database_path = db.db_path.clone();
        drop(db);

        let reopened = SourceDatabase::open_for_source_write_with_profile_guard(
            source_root.path(),
            &profile,
            database_root.path(),
        )
        .expect("reopen source database");
        let error = scan_page(&reopened, Some(cursor)).expect_err("old cursor must be stale");
        assert!(matches!(error, LegacyJournalScanError::StaleCursor { .. }));
        assert!(database_path.exists());

        drop(reopened);
        drop(profile);
        drop(app_root_guard);
        drop(app_root);
        drop(source_root);
        drop(database_root);
        drop(runtime);
    }

    #[test]
    fn snapshot_keeps_same_length_concurrent_commit_outside_page_view() {
        let fixture = ReaderFixture::new();
        insert_copy(&fixture.db, None, "same-length", "intent", "first.wav");
        let fence = fixture.db.source_publication_fence().expect("source fence");
        let transaction = fixture
            .db
            .connection
            .unchecked_transaction()
            .expect("reader snapshot");
        let upper = load_upper_rowid(&transaction)
            .expect("upper rowid")
            .expect("row");
        let candidates = load_candidates(&transaction, upper, None).expect("metadata preflight");
        let source_binding = fixture
            .db
            .legacy_journal_dedup_binding()
            .expect("dedup binding");
        let schema = load_legacy_schema(&transaction).expect("legacy schema");

        let writer = Connection::open(&fixture.db.db_path).expect("second connection");
        writer
            .busy_timeout(Duration::from_secs(1))
            .expect("writer timeout");
        writer
            .execute(
                "UPDATE file_ops_journal SET target_relative = 'other.wav' WHERE id = 'same-length'",
                [],
            )
            .expect("same-length concurrent update");
        drop(writer);

        let loaded = load_bounded_row(&transaction, &candidates[0], &source_binding, &schema)
            .expect("snapshot row");
        match loaded {
            BoundedRow::Decoded { entry, .. } => {
                assert_eq!(entry.target_relative, PathBuf::from("first.wav"));
            }
            other => panic!("snapshot unexpectedly retained row: {other:?}"),
        }
        transaction.rollback().expect("reader rollback");
        assert!(fence.is_live());

        let page = scan_page(&fixture.db, None).expect("post-commit page");
        assert!(matches!(
            page.entries()[0].outcome(),
            LegacyJournalRowOutcome::Decoded(entry)
                if entry.target_relative == PathBuf::from("other.wav")
        ));
    }

    #[test]
    fn scan_does_not_renew_source_lease() {
        let fixture = ReaderFixture::new();
        insert_copy(
            &fixture.db,
            None,
            "lease-observation",
            "intent",
            "lease.wav",
        );
        let before = fixture
            .db
            .source_lease_control_total_changes_for_tests()
            .expect("source lease")
            .expect("lease changes");
        scan_page(&fixture.db, None).expect("read-only page");
        let after = fixture
            .db
            .source_lease_control_total_changes_for_tests()
            .expect("source lease")
            .expect("lease changes");
        assert_eq!(before, after);
    }

    #[test]
    fn preview_bound_is_measured_in_utf8_bytes() {
        let multibyte = "é".repeat(MAX_LEGACY_JOURNAL_PREVIEW_BYTES);
        let preview = bounded_text(&multibyte);
        assert!(preview.len() <= MAX_LEGACY_JOURNAL_PREVIEW_BYTES);
        assert!(preview.len() < multibyte.len());

        let invalid = preview_text(Some(&[0xff; MAX_LEGACY_JOURNAL_PREVIEW_BYTES]));
        assert!(invalid.len() <= MAX_LEGACY_JOURNAL_PREVIEW_BYTES);
    }

    #[test]
    fn long_legacy_id_uses_a_bounded_key_preview() {
        let fixture = ReaderFixture::new();
        let long_id = "i".repeat(MAX_LEGACY_JOURNAL_PREVIEW_BYTES + 1);
        insert_copy(&fixture.db, None, &long_id, "intent", "long-id.wav");

        let page = scan_page(&fixture.db, None).expect("long id page");
        let key = page.entries()[0].key();
        assert!(
            matches!(key, LegacyJournalKey::TextPreview(value) if value.len() <= MAX_LEGACY_JOURNAL_PREVIEW_BYTES)
        );
    }

    #[test]
    fn active_caller_transaction_is_rejected_without_consuming_it() {
        let fixture = ReaderFixture::new();
        insert_copy(
            &fixture.db,
            None,
            "caller-transaction",
            "intent",
            "caller.wav",
        );
        let transaction = fixture
            .db
            .connection
            .unchecked_transaction()
            .expect("caller transaction");
        let error = scan_page(&fixture.db, None).expect_err("nested transaction");
        assert!(matches!(error, LegacyJournalScanError::SourceDb(_)));
        transaction.rollback().expect("caller rollback");
    }
}
