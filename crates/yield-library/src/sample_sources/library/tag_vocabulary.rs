//! Bounded profile-owned tag vocabulary persistence.

use rusqlite::{OptionalExtension, Transaction, params, params_from_iter};
use thiserror::Error;

use super::connection::LibraryDatabase;
use super::error::map_sql_error;
use super::{LibraryError, PROFILE_TAG_VOCABULARY_REVISION_KEY};

/// Maximum UTF-8 byte length of one profile vocabulary label.
pub const PROFILE_TAG_VOCABULARY_MAX_LABEL_BYTES: usize = 1024;
/// Maximum number of exact labels accepted by one vocabulary lookup.
pub const PROFILE_TAG_VOCABULARY_MAX_LOOKUP_LABELS: usize = 64;
/// Maximum number of rows returned by one vocabulary page.
pub const PROFILE_TAG_VOCABULARY_PAGE_MAX_PAGE_SIZE: usize = 64;

/// Stable profile vocabulary categories.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ProfileTagCategory {
    /// Playback behavior tags such as `loop` and `one-shot`.
    PlaybackType,
    /// Sound source or role tags such as `kick` and `vocal`.
    SoundType,
    /// Character tags such as `warm` and `bright`.
    Character,
    /// Prefix and source naming tags such as `artist` and `pack`.
    Prefix,
    /// Tuning and scale tags such as `major` and `dorian`.
    TuningScale,
}

impl ProfileTagCategory {
    /// Return the stable persisted category identifier.
    pub const fn id(self) -> &'static str {
        match self {
            Self::PlaybackType => "playback-type",
            Self::SoundType => "sound-type",
            Self::Character => "character",
            Self::Prefix => "prefix",
            Self::TuningScale => "tuning-scale",
        }
    }

    /// Resolve one exact persisted category identifier.
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "playback-type" => Some(Self::PlaybackType),
            "sound-type" => Some(Self::SoundType),
            "character" => Some(Self::Character),
            "prefix" => Some(Self::Prefix),
            "tuning-scale" => Some(Self::TuningScale),
            _ => None,
        }
    }
}

/// One profile vocabulary row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileTagVocabularyEntry {
    /// Canonical label identity.
    pub label: String,
    /// Category assigned to the label.
    pub category: ProfileTagCategory,
}

/// Exact-label lookup result at one observed vocabulary revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileTagVocabularyLookup {
    /// Vocabulary revision observed by the lookup.
    pub revision: u64,
    /// Matching rows, ordered by canonical label.
    pub entries: Vec<ProfileTagVocabularyEntry>,
}

/// Opaque continuation for a bounded literal-prefix vocabulary page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileTagVocabularyPageCursor {
    revision: u64,
    prefix: String,
    last_label: String,
}

/// One bounded literal-prefix vocabulary page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileTagVocabularyPage {
    /// Vocabulary revision observed by the page.
    pub revision: u64,
    /// Rows in binary canonical-label order.
    pub entries: Vec<ProfileTagVocabularyEntry>,
    /// Opaque continuation for another page, when more rows exist.
    pub next_cursor: Option<ProfileTagVocabularyPageCursor>,
}

/// Result of one revision-fenced vocabulary mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileTagVocabularySetReceipt {
    /// Vocabulary revision after the mutation.
    pub revision: u64,
    /// Whether the row was inserted or changed.
    pub changed: bool,
}

/// Result of one revision-fenced profile vocabulary deletion.
///
/// When `removed` is present it is the bounded before-image of the deleted row. A missing label
/// is an idempotent no-op at the requested revision and leaves the revision unchanged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileTagVocabularyDeleteReceipt {
    /// Vocabulary revision after the deletion, or the unchanged revision for a no-op.
    pub revision: u64,
    /// The deleted row before the mutation, when the label existed.
    pub removed: Option<ProfileTagVocabularyEntry>,
}

/// Input rejected before a profile vocabulary command enters the owner queue.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ProfileTagVocabularyInputError {
    /// An exact-label lookup contained too many labels.
    #[error("profile vocabulary lookup contains {actual} labels; maximum is {max}")]
    TooManyLookupLabels {
        /// Maximum number of exact labels.
        max: usize,
        /// Supplied number of labels.
        actual: usize,
    },
    /// A label was empty.
    #[error("profile vocabulary label is empty")]
    EmptyLabel,
    /// A label exceeded the byte bound.
    #[error("profile vocabulary label is {actual} bytes; maximum is {max}")]
    LabelTooLong {
        /// Maximum label byte length.
        max: usize,
        /// Supplied label byte length.
        actual: usize,
    },
    /// A label contained an embedded NUL.
    #[error("profile vocabulary label contains an embedded NUL byte")]
    LabelContainsNul,
    /// A label did not use the canonical ASCII spelling.
    #[error("profile vocabulary label is not canonical")]
    LabelNotCanonical,
    /// A prefix exceeded the byte bound.
    #[error("profile vocabulary prefix is {actual} bytes; maximum is {max}")]
    PrefixTooLong {
        /// Maximum prefix byte length.
        max: usize,
        /// Supplied prefix byte length.
        actual: usize,
    },
    /// A prefix contained an embedded NUL.
    #[error("profile vocabulary prefix contains an embedded NUL byte")]
    PrefixContainsNul,
    /// A prefix did not use the canonical ASCII spelling.
    #[error("profile vocabulary prefix is not canonical")]
    PrefixNotCanonical,
    /// A locked playback label was assigned to another category.
    #[error("locked playback label must use the playback-type category")]
    LockedPlaybackLabelRequiresPlaybackType,
    /// An arbitrary label was assigned to the locked playback category.
    #[error("only locked playback labels may use the playback-type category")]
    PlaybackTypeLabelNotAllowed,
    /// A locked playback label cannot be removed from the profile vocabulary.
    #[error("locked playback labels cannot be deleted")]
    LockedPlaybackLabelCannotBeDeleted,
}

impl ProfileTagVocabularyPageCursor {
    fn new(revision: u64, prefix: &str, last_label: &str) -> Self {
        Self {
            revision,
            prefix: String::from(prefix),
            last_label: String::from(last_label),
        }
    }
}

pub(super) fn validate_lookup_labels(
    labels: &mut Vec<String>,
) -> Result<(), ProfileTagVocabularyInputError> {
    validate_lookup_label_slice(labels)?;
    deduplicate_labels(labels);
    Ok(())
}

pub(super) fn validate_page_inputs(
    prefix: &str,
    cursor: Option<&ProfileTagVocabularyPageCursor>,
) -> Result<(), ProfileTagVocabularyInputError> {
    validate_prefix(prefix)?;
    if let Some(cursor) = cursor {
        validate_prefix(&cursor.prefix)?;
        validate_label(&cursor.last_label)?;
    }
    Ok(())
}

pub(super) fn validate_set_input(
    label: &str,
    category: ProfileTagCategory,
) -> Result<(), ProfileTagVocabularyInputError> {
    validate_label(label)?;
    validate_label_category(label, category)
}

pub(super) fn validate_delete_input(label: &str) -> Result<(), ProfileTagVocabularyInputError> {
    validate_label(label)?;
    if is_locked_playback_label(label) {
        return Err(ProfileTagVocabularyInputError::LockedPlaybackLabelCannotBeDeleted);
    }
    Ok(())
}

fn validate_label(label: &str) -> Result<(), ProfileTagVocabularyInputError> {
    if label.is_empty() {
        return Err(ProfileTagVocabularyInputError::EmptyLabel);
    }
    if label.len() > PROFILE_TAG_VOCABULARY_MAX_LABEL_BYTES {
        return Err(ProfileTagVocabularyInputError::LabelTooLong {
            max: PROFILE_TAG_VOCABULARY_MAX_LABEL_BYTES,
            actual: label.len(),
        });
    }
    if label.contains('\0') {
        return Err(ProfileTagVocabularyInputError::LabelContainsNul);
    }
    if !is_canonical_label(label, false) {
        return Err(ProfileTagVocabularyInputError::LabelNotCanonical);
    }
    Ok(())
}

fn validate_lookup_label_slice(labels: &[String]) -> Result<(), ProfileTagVocabularyInputError> {
    if labels.len() > PROFILE_TAG_VOCABULARY_MAX_LOOKUP_LABELS {
        return Err(ProfileTagVocabularyInputError::TooManyLookupLabels {
            max: PROFILE_TAG_VOCABULARY_MAX_LOOKUP_LABELS,
            actual: labels.len(),
        });
    }
    for label in labels {
        validate_label(label)?;
    }
    Ok(())
}

fn validate_prefix(prefix: &str) -> Result<(), ProfileTagVocabularyInputError> {
    if prefix.len() > PROFILE_TAG_VOCABULARY_MAX_LABEL_BYTES {
        return Err(ProfileTagVocabularyInputError::PrefixTooLong {
            max: PROFILE_TAG_VOCABULARY_MAX_LABEL_BYTES,
            actual: prefix.len(),
        });
    }
    if prefix.contains('\0') {
        return Err(ProfileTagVocabularyInputError::PrefixContainsNul);
    }
    if !is_canonical_label(prefix, true) {
        return Err(ProfileTagVocabularyInputError::PrefixNotCanonical);
    }
    Ok(())
}

fn is_canonical_label(value: &str, allow_empty: bool) -> bool {
    if value.is_empty() {
        return allow_empty;
    }

    let bytes = value.as_bytes();
    let mut previous_hyphen = false;
    for (index, byte) in bytes.iter().copied().enumerate() {
        match byte {
            b'a'..=b'z' | b'0'..=b'9' | b'_' => previous_hyphen = false,
            b'-' if index > 0 && (allow_empty || index + 1 < bytes.len()) && !previous_hyphen => {
                previous_hyphen = true;
            }
            _ => return false,
        }
    }
    true
}

fn validate_label_category(
    label: &str,
    category: ProfileTagCategory,
) -> Result<(), ProfileTagVocabularyInputError> {
    if is_locked_playback_label(label) {
        if category == ProfileTagCategory::PlaybackType {
            return Ok(());
        }
        return Err(ProfileTagVocabularyInputError::LockedPlaybackLabelRequiresPlaybackType);
    }
    if category == ProfileTagCategory::PlaybackType {
        return Err(ProfileTagVocabularyInputError::PlaybackTypeLabelNotAllowed);
    }
    Ok(())
}

fn is_locked_playback_label(label: &str) -> bool {
    if matches!(label, "loop" | "oneshot") {
        return true;
    }
    let mut words = label.split(|character| character == '-' || character == '_');
    let Some(first) = words.find(|word| !word.is_empty()) else {
        return false;
    };
    let Some(second) = words.find(|word| !word.is_empty()) else {
        return false;
    };
    first == "one" && second == "shot" && words.all(|word| word.is_empty())
}

fn deduplicate_labels(labels: &mut Vec<String>) {
    let mut unique = Vec::with_capacity(labels.len());
    for label in labels.drain(..) {
        if !unique.iter().any(|seen| seen == &label) {
            unique.push(label);
        }
    }
    *labels = unique;
}

fn unique_label_refs<'a>(labels: &'a [String]) -> Vec<&'a str> {
    let mut unique = Vec::with_capacity(labels.len());
    for label in labels {
        if !unique.iter().any(|seen| *seen == label.as_str()) {
            unique.push(label.as_str());
        }
    }
    unique
}

impl LibraryDatabase {
    pub(super) fn lookup_profile_tag_categories(
        &self,
        labels: &[String],
    ) -> Result<ProfileTagVocabularyLookup, LibraryError> {
        validate_lookup_label_slice(labels).map_err(LibraryError::from)?;
        let labels = unique_label_refs(labels);

        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(map_sql_error)?;
        let revision = profile_tag_vocabulary_revision(&transaction)?;
        if labels.is_empty() {
            transaction.rollback().map_err(map_sql_error)?;
            return Ok(ProfileTagVocabularyLookup {
                revision,
                entries: Vec::new(),
            });
        }

        let sql = profile_tag_lookup_sql(labels.len());
        let mut statement = transaction.prepare(&sql).map_err(map_sql_error)?;
        let mut rows = statement
            .query(params_from_iter(labels.iter().copied()))
            .map_err(map_sql_error)?;
        let mut entries = Vec::new();
        while let Some(row) = rows.next().map_err(map_sql_error)? {
            entries.push(profile_tag_entry_from_row(row)?);
        }
        drop(rows);
        drop(statement);
        transaction.rollback().map_err(map_sql_error)?;
        Ok(ProfileTagVocabularyLookup { revision, entries })
    }

    pub(super) fn profile_tag_vocabulary_page(
        &self,
        expected_revision: Option<u64>,
        prefix: &str,
        after_cursor: Option<&ProfileTagVocabularyPageCursor>,
        requested_limit: usize,
    ) -> Result<ProfileTagVocabularyPage, LibraryError> {
        validate_page_inputs(prefix, after_cursor).map_err(LibraryError::from)?;
        let limit = requested_limit.clamp(1, PROFILE_TAG_VOCABULARY_PAGE_MAX_PAGE_SIZE);
        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(map_sql_error)?;
        let revision = profile_tag_vocabulary_revision(&transaction)?;
        if let Some(expected_revision) = expected_revision {
            if expected_revision != revision {
                return Err(LibraryError::ProfileTagVocabularyStaleRevision {
                    expected: expected_revision,
                    actual: revision,
                });
            }
        }
        if let Some(cursor) = after_cursor {
            if cursor.revision != revision || cursor.prefix != prefix {
                return Err(LibraryError::ProfileTagVocabularyCursorMismatch);
            }
        }

        let mut entries = query_profile_tag_page(&transaction, prefix, after_cursor, limit)?;
        let has_more = entries.len() > limit;
        if has_more {
            entries.truncate(limit);
        }
        let next_cursor = has_more.then(|| {
            ProfileTagVocabularyPageCursor::new(
                revision,
                prefix,
                &entries
                    .last()
                    .expect("a page with continuation has a last entry")
                    .label,
            )
        });
        transaction.rollback().map_err(map_sql_error)?;
        Ok(ProfileTagVocabularyPage {
            revision,
            entries,
            next_cursor,
        })
    }

    pub(super) fn set_profile_tag_category(
        &mut self,
        expected_revision: u64,
        label: &str,
        category: ProfileTagCategory,
    ) -> Result<ProfileTagVocabularySetReceipt, LibraryError> {
        validate_set_input(label, category).map_err(LibraryError::from)?;
        let transaction = self.connection.transaction().map_err(map_sql_error)?;
        let revision = profile_tag_vocabulary_revision(&transaction)?;
        if revision != expected_revision {
            return Err(LibraryError::ProfileTagVocabularyStaleRevision {
                expected: expected_revision,
                actual: revision,
            });
        }

        let existing_code = transaction
            .query_row(
                "SELECT CASE category
                    WHEN 'playback-type' THEN 0
                    WHEN 'sound-type' THEN 1
                    WHEN 'character' THEN 2
                    WHEN 'prefix' THEN 3
                    WHEN 'tuning-scale' THEN 4
                    ELSE -1
                 END
                 FROM profile_tag_vocabulary
                 WHERE label = ?1",
                [label],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(map_sql_error)?;
        let existing = existing_code
            .map(profile_tag_category_from_code)
            .transpose()?;
        if let Some(existing) = existing {
            validate_stored_entry(label, existing)?;
            if existing == category {
                transaction.rollback().map_err(map_sql_error)?;
                return Ok(ProfileTagVocabularySetReceipt {
                    revision,
                    changed: false,
                });
            }
            transaction
                .execute(
                    "UPDATE profile_tag_vocabulary SET category = ?1 WHERE label = ?2",
                    params![category.id(), label],
                )
                .map_err(map_sql_error)?;
        } else {
            transaction
                .execute(
                    "INSERT INTO profile_tag_vocabulary (label, category) VALUES (?1, ?2)",
                    params![label, category.id()],
                )
                .map_err(map_sql_error)?;
        }

        let new_revision = revision
            .checked_add(1)
            .ok_or(LibraryError::ProfileTagVocabularyRevisionOverflow)?;
        set_profile_tag_vocabulary_revision(&transaction, new_revision)?;
        transaction.commit().map_err(map_sql_error)?;
        Ok(ProfileTagVocabularySetReceipt {
            revision: new_revision,
            changed: true,
        })
    }

    pub(super) fn delete_profile_tag_vocabulary(
        &mut self,
        expected_revision: u64,
        label: &str,
    ) -> Result<ProfileTagVocabularyDeleteReceipt, LibraryError> {
        validate_delete_input(label).map_err(LibraryError::from)?;
        let transaction = self.connection.transaction().map_err(map_sql_error)?;
        let revision = profile_tag_vocabulary_revision(&transaction)?;
        if revision != expected_revision {
            return Err(LibraryError::ProfileTagVocabularyStaleRevision {
                expected: expected_revision,
                actual: revision,
            });
        }

        let existing_code = transaction
            .query_row(
                "SELECT CASE category
                    WHEN 'playback-type' THEN 0
                    WHEN 'sound-type' THEN 1
                    WHEN 'character' THEN 2
                    WHEN 'prefix' THEN 3
                    WHEN 'tuning-scale' THEN 4
                    ELSE -1
                 END
                 FROM profile_tag_vocabulary
                 WHERE label = ?1",
                [label],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(map_sql_error)?;
        let Some(existing) = existing_code
            .map(profile_tag_category_from_code)
            .transpose()?
        else {
            transaction.rollback().map_err(map_sql_error)?;
            return Ok(ProfileTagVocabularyDeleteReceipt {
                revision,
                removed: None,
            });
        };
        validate_stored_entry(label, existing)?;
        let removed = ProfileTagVocabularyEntry {
            label: String::from(label),
            category: existing,
        };

        transaction
            .execute(
                "DELETE FROM profile_tag_vocabulary WHERE label = ?1",
                [label],
            )
            .map_err(map_sql_error)?;
        let new_revision = revision
            .checked_add(1)
            .ok_or(LibraryError::ProfileTagVocabularyRevisionOverflow)?;
        set_profile_tag_vocabulary_revision(&transaction, new_revision)?;
        transaction.commit().map_err(map_sql_error)?;
        Ok(ProfileTagVocabularyDeleteReceipt {
            revision: new_revision,
            removed: Some(removed),
        })
    }
}

fn profile_tag_lookup_sql(label_count: usize) -> String {
    let placeholders = (0..label_count).map(|_| "?").collect::<Vec<_>>().join(", ");
    format!(
        "SELECT {label}, {category}
         FROM profile_tag_vocabulary
         WHERE label IN ({placeholders})
         ORDER BY label COLLATE BINARY ASC",
        label = bounded_label_sql("label"),
        category = category_sql("category"),
    )
}

fn query_profile_tag_page(
    transaction: &Transaction<'_>,
    prefix: &str,
    after_cursor: Option<&ProfileTagVocabularyPageCursor>,
    limit: usize,
) -> Result<Vec<ProfileTagVocabularyEntry>, LibraryError> {
    let fetch_limit = i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX);
    let entries = if prefix.is_empty() {
        let sql = if after_cursor.is_some() {
            format!(
                "SELECT {label}, {category}
                 FROM profile_tag_vocabulary
                 WHERE label > ?1 COLLATE BINARY
                 ORDER BY label COLLATE BINARY ASC
                 LIMIT ?2",
                label = bounded_label_sql("label"),
                category = category_sql("category"),
            )
        } else {
            format!(
                "SELECT {label}, {category}
                 FROM profile_tag_vocabulary
                 ORDER BY label COLLATE BINARY ASC
                 LIMIT ?1",
                label = bounded_label_sql("label"),
                category = category_sql("category"),
            )
        };
        let mut statement = transaction.prepare(&sql).map_err(map_sql_error)?;
        if let Some(cursor) = after_cursor {
            read_profile_tag_entries(&mut statement, params![cursor.last_label, fetch_limit])?
        } else {
            read_profile_tag_entries(&mut statement, params![fetch_limit])?
        }
    } else {
        let upper_bound = profile_tag_prefix_upper_bound(prefix)
            .expect("a non-empty canonical prefix has a binary upper bound");
        let sql = if after_cursor.is_some() {
            format!(
                "SELECT {label}, {category}
                 FROM profile_tag_vocabulary
                 WHERE label > ?1 COLLATE BINARY
                   AND label < ?2 COLLATE BINARY
                 ORDER BY label COLLATE BINARY ASC
                 LIMIT ?3",
                label = bounded_label_sql("label"),
                category = category_sql("category"),
            )
        } else {
            format!(
                "SELECT {label}, {category}
                 FROM profile_tag_vocabulary
                 WHERE label >= ?1 COLLATE BINARY
                   AND label < ?2 COLLATE BINARY
                 ORDER BY label COLLATE BINARY ASC
                 LIMIT ?3",
                label = bounded_label_sql("label"),
                category = category_sql("category"),
            )
        };
        let mut statement = transaction.prepare(&sql).map_err(map_sql_error)?;
        if let Some(cursor) = after_cursor {
            read_profile_tag_entries(
                &mut statement,
                params![cursor.last_label, upper_bound, fetch_limit],
            )?
        } else {
            read_profile_tag_entries(&mut statement, params![prefix, upper_bound, fetch_limit])?
        }
    };
    Ok(entries)
}

fn read_profile_tag_entries<P: rusqlite::Params>(
    statement: &mut rusqlite::Statement<'_>,
    params: P,
) -> Result<Vec<ProfileTagVocabularyEntry>, LibraryError> {
    let mut rows = statement.query(params).map_err(map_sql_error)?;
    let mut entries = Vec::new();
    while let Some(row) = rows.next().map_err(map_sql_error)? {
        entries.push(profile_tag_entry_from_row(row)?);
    }
    Ok(entries)
}

fn profile_tag_entry_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<ProfileTagVocabularyEntry, LibraryError> {
    let label = row
        .get::<_, Option<String>>(0)
        .map_err(map_sql_error)?
        .ok_or(LibraryError::ProfileTagVocabularyMalformedData {
            reason: "stored label exceeds its bound or contains NUL",
        })?;
    let category_code = row.get::<_, i64>(1).map_err(map_sql_error)?;
    let category = profile_tag_category_from_code(category_code)?;
    validate_stored_entry(&label, category)?;
    Ok(ProfileTagVocabularyEntry { label, category })
}

fn validate_stored_entry(label: &str, category: ProfileTagCategory) -> Result<(), LibraryError> {
    if validate_label(label).is_err() {
        return Err(LibraryError::ProfileTagVocabularyMalformedData {
            reason: "stored label is not canonical",
        });
    }
    if validate_label_category(label, category).is_err() {
        return Err(LibraryError::ProfileTagVocabularyMalformedData {
            reason: "stored category does not match locked-label policy",
        });
    }
    Ok(())
}

fn profile_tag_category_from_code(code: i64) -> Result<ProfileTagCategory, LibraryError> {
    match code {
        0 => Ok(ProfileTagCategory::PlaybackType),
        1 => Ok(ProfileTagCategory::SoundType),
        2 => Ok(ProfileTagCategory::Character),
        3 => Ok(ProfileTagCategory::Prefix),
        4 => Ok(ProfileTagCategory::TuningScale),
        _ => Err(LibraryError::ProfileTagVocabularyMalformedData {
            reason: "stored category is not a known category identifier",
        }),
    }
}

fn profile_tag_vocabulary_revision(transaction: &Transaction<'_>) -> Result<u64, LibraryError> {
    const MAX_REVISION_BYTES: usize = 20;
    let Some((actual, bytes)) = transaction
        .query_row(
            "SELECT length(CAST(value AS BLOB)),
                    CASE
                        WHEN typeof(value) = 'text'
                             AND length(CAST(value AS BLOB)) <= ?2
                        THEN CAST(value AS BLOB)
                    END
             FROM metadata
             WHERE key = ?1",
            params![
                PROFILE_TAG_VOCABULARY_REVISION_KEY,
                MAX_REVISION_BYTES as i64
            ],
            |row| {
                Ok((
                    row.get::<_, Option<i64>>(0)?,
                    row.get::<_, Option<Vec<u8>>>(1)?,
                ))
            },
        )
        .optional()
        .map_err(map_sql_error)?
    else {
        return Ok(0);
    };
    let Some(actual) = actual else {
        return Err(LibraryError::ProfileTagVocabularyRevisionMalformed);
    };
    let actual = usize::try_from(actual).map_err(|_| LibraryError::MetadataTooLarge {
        key: PROFILE_TAG_VOCABULARY_REVISION_KEY,
        limit: MAX_REVISION_BYTES,
        actual: usize::MAX,
    })?;
    if actual > MAX_REVISION_BYTES {
        return Err(LibraryError::MetadataTooLarge {
            key: PROFILE_TAG_VOCABULARY_REVISION_KEY,
            limit: MAX_REVISION_BYTES,
            actual,
        });
    }
    let Some(bytes) = bytes else {
        return Err(LibraryError::ProfileTagVocabularyRevisionMalformed);
    };
    let value = String::from_utf8(bytes).map_err(|_| LibraryError::MetadataInvalidUtf8 {
        key: PROFILE_TAG_VOCABULARY_REVISION_KEY,
    })?;
    value
        .parse::<u64>()
        .map_err(|_| LibraryError::ProfileTagVocabularyRevisionMalformed)
}

fn set_profile_tag_vocabulary_revision(
    transaction: &Transaction<'_>,
    revision: u64,
) -> Result<(), LibraryError> {
    transaction
        .execute(
            "INSERT INTO metadata (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![PROFILE_TAG_VOCABULARY_REVISION_KEY, revision.to_string()],
        )
        .map_err(map_sql_error)?;
    Ok(())
}

fn bounded_label_sql(column: &str) -> String {
    format!(
        "CASE
            WHEN typeof({column}) = 'text'
                 AND length(CAST({column} AS BLOB)) BETWEEN 1 AND {max_bytes}
                 AND instr(CAST({column} AS TEXT), char(0)) = 0
            THEN CAST({column} AS TEXT)
         END",
        max_bytes = PROFILE_TAG_VOCABULARY_MAX_LABEL_BYTES,
    )
}

fn category_sql(column: &str) -> String {
    format!(
        "CASE {column}
            WHEN 'playback-type' THEN 0
            WHEN 'sound-type' THEN 1
            WHEN 'character' THEN 2
            WHEN 'prefix' THEN 3
            WHEN 'tuning-scale' THEN 4
            ELSE -1
         END",
        column = column,
    )
}

fn profile_tag_prefix_upper_bound(prefix: &str) -> Option<String> {
    let mut bytes = prefix.as_bytes().to_vec();
    while bytes.last().is_some_and(|byte| *byte == u8::MAX) {
        bytes.pop();
    }
    let last = bytes.last_mut()?;
    *last = last.checked_add(1)?;
    Some(String::from_utf8(bytes).expect("canonical profile prefixes are ASCII"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_dirs::ConfigBaseGuard;
    use crate::test_runtime::TestRuntimeGuard;
    use rusqlite::params;
    use tempfile::tempdir;

    fn open_database(directory: &tempfile::TempDir) -> (ConfigBaseGuard, LibraryDatabase) {
        let base = std::fs::canonicalize(directory.path()).expect("canonical test base");
        let guard = ConfigBaseGuard::set(base);
        let database = LibraryDatabase::open().expect("open library database");
        (guard, database)
    }

    #[test]
    fn exact_lookup_uses_the_vocabulary_primary_key() {
        let _runtime = TestRuntimeGuard::acquire();
        let directory = tempdir().expect("test directory");
        let (_guard, database) = open_database(&directory);
        let sql = format!("EXPLAIN QUERY PLAN {}", profile_tag_lookup_sql(2));
        let mut statement = database.connection.prepare(&sql).unwrap();
        let details = statement
            .query_map(params!["kick", "snare"], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(
            details.iter().any(|detail| {
                detail.contains("SEARCH profile_tag_vocabulary") && detail.contains("PRIMARY KEY")
            }),
            "{details:?}"
        );
    }

    #[test]
    fn vocabulary_mutations_are_revision_fenced_and_idempotent() {
        let _runtime = TestRuntimeGuard::acquire();
        let directory = tempdir().expect("test directory");
        let (_guard, mut database) = open_database(&directory);

        let inserted = database
            .set_profile_tag_category(0, "kick", ProfileTagCategory::SoundType)
            .expect("insert label");
        assert_eq!(
            inserted,
            ProfileTagVocabularySetReceipt {
                revision: 1,
                changed: true
            }
        );

        let unchanged = database
            .set_profile_tag_category(1, "kick", ProfileTagCategory::SoundType)
            .expect("idempotent label");
        assert_eq!(
            unchanged,
            ProfileTagVocabularySetReceipt {
                revision: 1,
                changed: false
            }
        );

        let changed = database
            .set_profile_tag_category(1, "kick", ProfileTagCategory::Character)
            .expect("change label category");
        assert_eq!(
            changed,
            ProfileTagVocabularySetReceipt {
                revision: 2,
                changed: true
            }
        );

        let stale = database
            .set_profile_tag_category(1, "snare", ProfileTagCategory::SoundType)
            .expect_err("stale mutation");
        assert!(matches!(
            stale,
            LibraryError::ProfileTagVocabularyStaleRevision {
                expected: 1,
                actual: 2
            }
        ));

        let lookup = database
            .lookup_profile_tag_categories(&[
                String::from("kick"),
                String::from("kick"),
                String::from("missing"),
            ])
            .expect("lookup labels");
        assert_eq!(lookup.revision, 2);
        assert_eq!(lookup.entries.len(), 1);
        assert_eq!(lookup.entries[0].label, "kick");
        assert_eq!(lookup.entries[0].category, ProfileTagCategory::Character);
    }

    #[test]
    fn vocabulary_deletion_returns_before_image_and_is_revision_fenced() {
        let _runtime = TestRuntimeGuard::acquire();
        let directory = tempdir().expect("test directory");
        let (_guard, mut database) = open_database(&directory);

        let inserted = database
            .set_profile_tag_category(0, "kick", ProfileTagCategory::SoundType)
            .expect("insert label");
        assert_eq!(inserted.revision, 1);

        assert!(matches!(
            database.delete_profile_tag_vocabulary(0, "kick"),
            Err(LibraryError::ProfileTagVocabularyStaleRevision {
                expected: 0,
                actual: 1
            })
        ));

        let deleted = database
            .delete_profile_tag_vocabulary(1, "kick")
            .expect("delete label");
        assert_eq!(
            deleted,
            ProfileTagVocabularyDeleteReceipt {
                revision: 2,
                removed: Some(ProfileTagVocabularyEntry {
                    label: String::from("kick"),
                    category: ProfileTagCategory::SoundType,
                }),
            }
        );

        let missing = database
            .delete_profile_tag_vocabulary(2, "kick")
            .expect("missing delete is a no-op");
        assert_eq!(
            missing,
            ProfileTagVocabularyDeleteReceipt {
                revision: 2,
                removed: None,
            }
        );

        let restored = database
            .set_profile_tag_category(
                deleted.revision,
                "kick",
                deleted.removed.as_ref().expect("before image").category,
            )
            .expect("restore before image");
        assert_eq!(restored.revision, 3);

        drop(database);
        let (_guard, reopened) = open_database(&directory);
        let lookup = reopened
            .lookup_profile_tag_categories(&[String::from("kick")])
            .expect("reopened vocabulary");
        assert_eq!(lookup.revision, 3);
        assert_eq!(lookup.entries.len(), 1);
        assert_eq!(lookup.entries[0].category, ProfileTagCategory::SoundType);
    }

    #[test]
    fn literal_prefix_pages_are_ordered_keyset_pages_and_revision_fenced() {
        let _runtime = TestRuntimeGuard::acquire();
        let directory = tempdir().expect("test directory");
        let (_guard, mut database) = open_database(&directory);
        let mut revision = 0;
        for label in ["a-one", "a_two", "a-z", "b"] {
            revision = database
                .set_profile_tag_category(revision, label, ProfileTagCategory::Character)
                .expect("insert vocabulary label")
                .revision;
        }

        let first = database
            .profile_tag_vocabulary_page(Some(revision), "a", None, 2)
            .expect("first page");
        assert_eq!(first.revision, revision);
        assert_eq!(
            first
                .entries
                .iter()
                .map(|entry| entry.label.as_str())
                .collect::<Vec<_>>(),
            ["a-one", "a-z"]
        );
        let cursor = first.next_cursor.clone().expect("continuation");

        let second = database
            .profile_tag_vocabulary_page(Some(revision), "a", Some(&cursor), 0)
            .expect("second page with clamped limit");
        assert_eq!(
            second
                .entries
                .iter()
                .map(|entry| entry.label.as_str())
                .collect::<Vec<_>>(),
            ["a_two"]
        );
        assert!(second.next_cursor.is_none());

        assert!(matches!(
            database.profile_tag_vocabulary_page(Some(revision), "b", Some(&cursor), 2),
            Err(LibraryError::ProfileTagVocabularyCursorMismatch)
        ));
        let new_revision = database
            .set_profile_tag_category(revision, "another", ProfileTagCategory::Character)
            .expect("advance revision")
            .revision;
        assert!(matches!(
            database.profile_tag_vocabulary_page(None, "a", first.next_cursor.as_ref(), 2),
            Err(LibraryError::ProfileTagVocabularyCursorMismatch)
        ));
        assert_eq!(new_revision, revision + 1);

        drop(database);
        let (_guard, reopened) = open_database(&directory);
        let page = reopened
            .profile_tag_vocabulary_page(None, "a", None, 64)
            .expect("reopened vocabulary");
        assert_eq!(page.revision, new_revision);
        assert_eq!(page.entries.len(), 4);
    }

    #[test]
    fn bounds_canonical_spelling_and_locked_playback_policy_are_checked_before_mutation() {
        let _runtime = TestRuntimeGuard::acquire();
        let directory = tempdir().expect("test directory");
        let (_guard, mut database) = open_database(&directory);

        let invalid = ["", "One-shot", "one--shot", "-loop", "loop-"];
        for label in invalid {
            assert!(matches!(
                database.set_profile_tag_category(0, label, ProfileTagCategory::Character),
                Err(LibraryError::InvalidProfileTagVocabularyInput(_))
            ));
        }
        assert!(matches!(
            database.set_profile_tag_category(0, "loop", ProfileTagCategory::Character),
            Err(LibraryError::InvalidProfileTagVocabularyInput(
                ProfileTagVocabularyInputError::LockedPlaybackLabelRequiresPlaybackType
            ))
        ));
        assert!(matches!(
            database.set_profile_tag_category(0, "kick", ProfileTagCategory::PlaybackType),
            Err(LibraryError::InvalidProfileTagVocabularyInput(
                ProfileTagVocabularyInputError::PlaybackTypeLabelNotAllowed
            ))
        ));
        assert!(matches!(
            database.set_profile_tag_category(0, "one_shot", ProfileTagCategory::PlaybackType),
            Ok(ProfileTagVocabularySetReceipt {
                revision: 1,
                changed: true
            })
        ));
        for label in ["loop", "oneshot", "one_shot", "one-shot"] {
            assert!(matches!(
                database.delete_profile_tag_vocabulary(1, label),
                Err(LibraryError::InvalidProfileTagVocabularyInput(
                    ProfileTagVocabularyInputError::LockedPlaybackLabelCannotBeDeleted
                ))
            ));
        }

        let too_long = "x".repeat(PROFILE_TAG_VOCABULARY_MAX_LABEL_BYTES + 1);
        assert!(matches!(
            database.lookup_profile_tag_categories(&[too_long]),
            Err(LibraryError::InvalidProfileTagVocabularyInput(
                ProfileTagVocabularyInputError::LabelTooLong { .. }
            ))
        ));
        let too_many = (0..=PROFILE_TAG_VOCABULARY_MAX_LOOKUP_LABELS)
            .map(|index| format!("label{index}"))
            .collect::<Vec<_>>();
        assert!(matches!(
            database.lookup_profile_tag_categories(&too_many),
            Err(LibraryError::InvalidProfileTagVocabularyInput(
                ProfileTagVocabularyInputError::TooManyLookupLabels { .. }
            ))
        ));
        assert_eq!(
            database
                .profile_tag_vocabulary_page(None, "", None, 64)
                .expect("valid rows remain")
                .entries
                .len(),
            1
        );
    }

    #[test]
    fn vocabulary_deletion_rolls_back_on_revision_overflow_and_malformed_rows() {
        let _runtime = TestRuntimeGuard::acquire();
        let directory = tempdir().expect("test directory");
        let (_guard, mut database) = open_database(&directory);
        database
            .set_profile_tag_category(0, "kick", ProfileTagCategory::SoundType)
            .expect("insert label");
        database
            .connection
            .execute(
                "UPDATE metadata SET value = ?1 WHERE key = ?2",
                params![u64::MAX.to_string(), PROFILE_TAG_VOCABULARY_REVISION_KEY],
            )
            .expect("set maximum revision");

        assert!(matches!(
            database.delete_profile_tag_vocabulary(u64::MAX, "kick"),
            Err(LibraryError::ProfileTagVocabularyRevisionOverflow)
        ));
        let unchanged = database
            .lookup_profile_tag_categories(&[String::from("kick")])
            .expect("lookup after rollback");
        assert_eq!(unchanged.revision, u64::MAX);
        assert_eq!(unchanged.entries.len(), 1);

        database
            .connection
            .execute_batch(
                "DROP TABLE profile_tag_vocabulary;
                 CREATE TABLE profile_tag_vocabulary (label TEXT PRIMARY KEY COLLATE BINARY, category TEXT);
                 INSERT INTO profile_tag_vocabulary(label, category) VALUES ('bad', 'unknown');",
            )
            .expect("corrupt vocabulary fixture");
        assert!(matches!(
            database.delete_profile_tag_vocabulary(u64::MAX, "bad"),
            Err(LibraryError::ProfileTagVocabularyMalformedData { .. })
        ));
        let row_count: i64 = database
            .connection
            .query_row(
                "SELECT count(*) FROM profile_tag_vocabulary WHERE label = 'bad'",
                [],
                |row| row.get(0),
            )
            .expect("corrupt row remains");
        assert_eq!(row_count, 1);
    }

    #[test]
    fn malformed_rows_are_rejected_before_authoritative_identity_is_returned() {
        let _runtime = TestRuntimeGuard::acquire();
        let directory = tempdir().expect("test directory");
        let (_guard, database) = open_database(&directory);
        database
            .connection
            .execute_batch(
                "DROP TABLE profile_tag_vocabulary;
                 CREATE TABLE profile_tag_vocabulary (label TEXT PRIMARY KEY COLLATE BINARY, category TEXT);
                 INSERT INTO profile_tag_vocabulary(label, category) VALUES ('good', 'character'), ('bad', 'unknown');",
            )
            .expect("corrupt vocabulary fixture");

        assert!(matches!(
            database.profile_tag_vocabulary_page(None, "", None, 64),
            Err(LibraryError::ProfileTagVocabularyMalformedData { .. })
        ));
        let lookup = database
            .lookup_profile_tag_categories(&[String::from("bad")])
            .expect_err("corrupt category");
        assert!(matches!(
            lookup,
            LibraryError::ProfileTagVocabularyMalformedData { .. }
        ));

        database
            .connection
            .execute("DELETE FROM profile_tag_vocabulary WHERE label = 'bad'", [])
            .unwrap();

        database
            .connection
            .execute(
                "INSERT INTO profile_tag_vocabulary(label, category) VALUES (?1, 'character')",
                params!["x".repeat(PROFILE_TAG_VOCABULARY_MAX_LABEL_BYTES + 1)],
            )
            .expect("overlong corrupt label fixture");
        assert!(matches!(
            database.profile_tag_vocabulary_page(None, "", None, 64),
            Err(LibraryError::ProfileTagVocabularyMalformedData { .. })
        ));
    }
}
