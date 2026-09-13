//! Bounded, read-only paging for the legacy source-local file-operation journal.
//!
//! The source-local journal predates the profile operation journal and has no observation
//! sequence.  This reader therefore uses a temporary SQLite rowid cursor and deliberately keeps
//! its result separate from the universal operation-journal recovery types.  A decoded legacy row
//! is evidence for a later compatibility decision; it is never an execution-ready operation.

use std::fmt::Display;

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use thiserror::Error;

use super::super::{SourceDatabase, SourceDbError, SourcePublicationFence};
use super::decode::{MalformedJournalEntry, decode_journal_row};
use super::entry::FileOpJournalEntry;

/// Maximum number of legacy journal rows returned by one page.
pub const MAX_LEGACY_JOURNAL_ROWS_PER_PAGE: usize = 16;
/// Maximum number of legacy text bytes materialized by one page.
pub const MAX_LEGACY_JOURNAL_BYTES_PER_PAGE: usize = 2 * 1024 * 1024;
/// Maximum number of bytes materialized from one legacy text field.
pub const MAX_LEGACY_JOURNAL_FIELD_BYTES: usize = 64 * 1024;
/// Maximum number of bytes retained in one legacy key or diagnostic preview.
pub const MAX_LEGACY_JOURNAL_PREVIEW_BYTES: usize = 256;

const PAGE_QUERY_LIMIT: i64 = (MAX_LEGACY_JOURNAL_ROWS_PER_PAGE + 1) as i64;
const LEGACY_COLUMN_COUNT: usize = 18;

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

/// One bounded legacy journal row observation.
#[derive(Clone, Debug)]
pub struct LegacyJournalEntry {
    rowid: i64,
    key: LegacyJournalKey,
    outcome: LegacyJournalRowOutcome,
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

    /// Return the text bytes materialized for this row.
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

    /// Return the aggregate text bytes materialized for this page.
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
    let page_result = scan_page_in_transaction(&transaction, cursor.as_ref(), &before_fence);
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

        if let Some(reason) = candidate.metadata_issue() {
            last_rowid = Some(candidate.rowid);
            entries.push(LegacyJournalEntry {
                rowid: candidate.rowid,
                key: candidate.key(),
                outcome: LegacyJournalRowOutcome::Retained(reason),
                bytes: 0,
            });
            continue;
        }

        let row_bytes = candidate.text_bytes()?;
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
                bytes: 0,
            });
            continue;
        }
        if bytes.saturating_add(row_bytes) > MAX_LEGACY_JOURNAL_BYTES_PER_PAGE {
            deferred = true;
            break;
        }

        let (outcome, loaded_bytes) = match load_bounded_row(transaction, candidate)? {
            BoundedRow::Missing => (
                LegacyJournalRowOutcome::Retained(LegacyJournalRetentionReason::RowDisappeared {
                    rowid: candidate.rowid,
                }),
                0,
            ),
            BoundedRow::Changed(reason) => (
                LegacyJournalRowOutcome::Retained(LegacyJournalRetentionReason::RowChanged {
                    rowid: candidate.rowid,
                    reason,
                }),
                0,
            ),
            BoundedRow::Decoded { entry, bytes } => {
                (LegacyJournalRowOutcome::Decoded(entry), bytes)
            }
            BoundedRow::Malformed { malformed, bytes } => (
                LegacyJournalRowOutcome::Retained(map_malformed(malformed)),
                bytes,
            ),
        };
        bytes = bytes.saturating_add(loaded_bytes);
        last_rowid = Some(candidate.rowid);
        entries.push(LegacyJournalEntry {
            rowid: candidate.rowid,
            key: candidate.key(),
            outcome,
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
}

#[derive(Debug)]
enum BoundedRow {
    Missing,
    Changed(String),
    Decoded {
        entry: FileOpJournalEntry,
        bytes: usize,
    },
    Malformed {
        malformed: MalformedJournalEntry,
        bytes: usize,
    },
}

fn load_bounded_row(
    connection: &Connection,
    candidate: &LegacyCandidate,
) -> Result<BoundedRow, LegacyJournalScanError> {
    let values = LEGACY_COLUMNS
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let descriptor = LegacyJournalField::from_index(index);
            if descriptor.is_text() {
                format!(
                    "CASE WHEN typeof({column}) = 'text' AND length(CAST({column} AS BLOB)) <= {MAX_LEGACY_JOURNAL_FIELD_BYTES} THEN {column} ELSE NULL END"
                )
            } else {
                format!(
                    "CASE WHEN typeof({column}) IN ('integer', 'null') THEN {column} ELSE NULL END"
                )
            }
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
        return Ok(BoundedRow::Missing);
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
        return Ok(BoundedRow::Changed(String::from(
            "legacy row metadata changed during bounded read",
        )));
    }
    let bytes = candidate.text_bytes()?;
    match decode_journal_row(row) {
        Ok(entry) => Ok(BoundedRow::Decoded { entry, bytes }),
        Err(malformed) => Ok(BoundedRow::Malformed { malformed, bytes }),
    }
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

        let loaded = load_bounded_row(&transaction, &candidates[0]).expect("snapshot row");
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
