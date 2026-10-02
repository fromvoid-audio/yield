use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params, types::Value};

use super::schema;
use super::util::{
    EXACT_SUBTREE_PATH_PREDICATE, INDEX_PATH_ENCODING_PLAIN, exact_subtree_path_bounds,
    map_sql_error, normalize_source_index_path, parse_source_index_path_from_db,
};
use super::{
    META_SOURCE_INDEX_REVISION, SourceDatabase, SourceDbError, SourceIndexClassification,
    SourceIndexDiagnostic, SourceIndexEntry, SourceIndexSnapshot, SourceWriteBatch,
};
use crate::sample_sources::reconciliation::RootRelativePath;

/// Maximum retained index entries returned by one page.
pub const SOURCE_INDEX_PAGE_MAX_ENTRIES: usize = 128;

/// Maximum aggregate stored variable-field bytes read per page, including lookahead.
pub const SOURCE_INDEX_PAGE_MAX_BYTES: usize = 128 * 1024;

/// Opaque, in-memory continuation bound to one open database and index revision.
///
/// Reopening the connection requires starting a new traversal. This cursor grants
/// no source lifecycle authority, scan completeness or filesystem/reveal permission.
#[derive(Clone, Debug)]
pub struct SourceIndexPageCursor {
    binding: std::sync::Arc<()>,
    root: PathBuf,
    revision: u64,
    after: String,
}

impl SourceIndexPageCursor {
    /// Retained variable input bytes for bounded owner admission, without exposing its key.
    pub fn input_bytes(&self) -> usize {
        self.root
            .as_os_str()
            .as_encoded_bytes()
            .len()
            .saturating_add(self.after.len())
    }
}

/// Bounded retained index facts, including unsupported and inaccessible entries.
///
/// An available schema and exhausted cursor do not prove a complete/current scan.
#[derive(Clone, Debug)]
pub struct SourceIndexPage {
    /// False when the legacy database has no index schema; entries are then unknown.
    pub schema_available: bool,
    /// Index revision observed in the same SQLite transaction as these entries.
    pub revision: u64,
    /// At most the requested limit, ordered by the lossless storage key.
    pub entries: Vec<SourceIndexEntry>,
    /// Continue at this revision, or none if the retained table is exhausted/unavailable.
    pub next: Option<SourceIndexPageCursor>,
    /// Private connection/page-bound inspection proposals for the corresponding rows.
    pub inspection_handles: Vec<super::SourceIndexEntryHandle>,
}

/// Explicit rejected continuation, limit, or database projection outcome.
#[derive(Debug, thiserror::Error)]
pub enum SourceIndexPageError {
    /// The limit is outside one through the fixed page maximum.
    #[error("source index page limit must be between 1 and {SOURCE_INDEX_PAGE_MAX_ENTRIES}")]
    InvalidLimit,
    /// Stored row fields or continuation exceed the bounded projection budget.
    #[error("source index page exceeds its variable-field byte budget")]
    ByteLimit,
    /// A cursor came from a different/reopened connection or retargeted source root.
    #[error("source index cursor belongs to a different database connection or source root")]
    ForeignCursor,
    /// The retained table changed during a traversal; start a new traversal.
    #[error("source index cursor expected revision {expected}, found {actual}")]
    StaleCursor {
        /// Revision bound to the cursor.
        expected: u64,
        /// Revision currently observed by the reader.
        actual: u64,
    },
    /// A read failed or a malformed retained row prevented a trustworthy projection.
    #[error(transparent)]
    Database(#[from] SourceDbError),
}

const REQUIRED_COLUMNS: [&str; 7] = [
    "path",
    "classification",
    "file_size",
    "modified_ns",
    "file_identity",
    "diagnostic",
    "format_policy_version",
];

impl SourceDatabase {
    /// Read one bounded keyset page without collecting the full retained index.
    ///
    /// Revision, schema and rows share one read transaction. A continuation is
    /// valid only for this open connection/root and unchanged index revision.
    /// Malformed rows fail the page rather than silently implying absence.
    /// Callers must separately establish source lifecycle and scan completeness.
    pub fn source_index_page(
        &self,
        cursor: Option<&SourceIndexPageCursor>,
        limit: usize,
    ) -> Result<SourceIndexPage, SourceIndexPageError> {
        if !(1..=SOURCE_INDEX_PAGE_MAX_ENTRIES).contains(&limit) {
            return Err(SourceIndexPageError::InvalidLimit);
        }
        if cursor.is_some_and(|cursor| {
            !std::sync::Arc::ptr_eq(&cursor.binding, &self.index_page_binding)
                || cursor.root != self.root
        }) {
            return Err(SourceIndexPageError::ForeignCursor);
        }
        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(map_sql_error)?;
        if !source_index_schema_available(&transaction)? {
            if cursor.is_some() {
                return Err(SourceDbError::Unexpected.into());
            }
            return Ok(SourceIndexPage {
                schema_available: false,
                revision: 0,
                entries: Vec::new(),
                next: None,
                inspection_handles: Vec::new(),
            });
        }
        let revision = read_index_revision(&transaction)?;
        if let Some(cursor) = cursor {
            if cursor.revision != revision {
                return Err(SourceIndexPageError::StaleCursor {
                    expected: cursor.revision,
                    actual: revision,
                });
            }
        }
        let encoding = if source_index_path_encoding_available(&transaction)? {
            "path_encoding"
        } else {
            "0 AS path_encoding"
        };
        let comparison = if cursor.is_some() { ">" } else { ">=" };
        let sql = format!(
            "SELECT path, {encoding}, classification, file_size, modified_ns, file_identity,
                    diagnostic, format_policy_version
             FROM source_index_entries WHERE path {comparison} ?1 COLLATE BINARY
             ORDER BY path COLLATE BINARY ASC LIMIT ?2"
        );
        let mut statement = transaction.prepare(&sql).map_err(map_sql_error)?;
        let mut rows = statement
            .query(params![
                cursor.map_or("", |cursor| cursor.after.as_str()),
                (limit + 1) as i64
            ])
            .map_err(map_sql_error)?;
        let mut field_bytes = self.root.as_os_str().as_encoded_bytes().len();
        if field_bytes > SOURCE_INDEX_PAGE_MAX_BYTES {
            return Err(SourceIndexPageError::ByteLimit);
        }
        let mut entries = Vec::with_capacity(limit + 1);
        let mut keys = Vec::with_capacity(limit + 1);
        while let Some(row) = rows.next().map_err(map_sql_error)? {
            // Borrow SQLite values before any String/path allocation, including lookahead.
            for column in [0, 2, 5, 6] {
                let bytes = match row.get_ref(column).map_err(map_sql_error)? {
                    rusqlite::types::ValueRef::Text(value)
                    | rusqlite::types::ValueRef::Blob(value) => value.len(),
                    _ => 0,
                };
                field_bytes = field_bytes.saturating_add(bytes);
                if field_bytes > SOURCE_INDEX_PAGE_MAX_BYTES {
                    return Err(SourceIndexPageError::ByteLimit);
                }
            }
            keys.push(row.get::<_, String>(0).map_err(map_sql_error)?);
            entries.push(strict_page_entry(row)?);
        }
        drop(rows);
        drop(statement);
        let next = if entries.len() > limit {
            entries.truncate(limit);
            Some(SourceIndexPageCursor {
                binding: self.index_page_binding.clone(),
                root: self.root.clone(),
                revision,
                after: keys[limit - 1].clone(),
            })
        } else {
            None
        };
        transaction.rollback().map_err(map_sql_error)?;
        let delivery = std::sync::Arc::new(());
        let inspection_handles = entries
            .iter()
            .map(|entry| {
                super::SourceIndexEntryHandle::new(
                    self.index_page_binding.clone(),
                    delivery.clone(),
                    revision,
                    entry.clone(),
                )
            })
            .collect();
        Ok(SourceIndexPage {
            inspection_handles,
            schema_available: true,
            revision,
            entries,
            next,
        })
    }

    /// Read index-only entries at exact validated paths and their current index revision.
    ///
    /// The row query remains equality-only and bounded by the supplied paths; the revision is
    /// read separately so callers can reject a concurrent index write before publishing facts.
    pub fn source_index_entries_for_exact_paths(
        &self,
        paths: &[RootRelativePath],
    ) -> Result<SourceIndexSnapshot, SourceDbError> {
        let expected_revision = if source_index_schema_available(&self.connection)? {
            read_index_revision(&self.connection)?
        } else {
            0
        };
        self.source_index_entries_for_exact_paths_at_revision(expected_revision, paths)
    }

    /// Read the complete index-only file set and its independent revision atomically.
    ///
    /// Legacy read-only databases without the table project revision zero and
    /// an empty set instead of requiring a migration on the reader.
    pub fn source_index_snapshot(&self) -> Result<SourceIndexSnapshot, SourceDbError> {
        if !source_index_schema_available(&self.connection)? {
            return Ok(SourceIndexSnapshot {
                revision: 0,
                entries: Vec::new(),
            });
        }
        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(map_sql_error)?;
        let revision = read_index_revision(&transaction)?;
        let path_encoding = source_index_path_encoding_available(&transaction)?;
        let entries = collect_entries(
            &transaction,
            &source_index_select_sql(path_encoding, false),
            [],
        )?;
        transaction.rollback().map_err(map_sql_error)?;
        Ok(SourceIndexSnapshot { revision, entries })
    }

    /// Read all durable index-only entries in deterministic path order.
    pub fn list_source_index_entries(&self) -> Result<Vec<SourceIndexEntry>, SourceDbError> {
        Ok(self.source_index_snapshot()?.entries)
    }

    /// Read index-only entries at or below one source-relative path.
    pub fn list_source_index_entries_under_path(
        &self,
        relative_path: &Path,
    ) -> Result<Vec<SourceIndexEntry>, SourceDbError> {
        if !source_index_schema_available(&self.connection)? {
            return Ok(Vec::new());
        }
        let (normalized, _path_encoding) = normalize_source_index_path(relative_path)?;
        let (lower_bound, upper_bound) = exact_subtree_path_bounds(&normalized);
        let path_encoding = source_index_path_encoding_available(&self.connection)?;
        collect_entries(
            &self.connection,
            &source_index_select_sql(path_encoding, true),
            params![normalized, lower_bound, upper_bound],
        )
    }
}

impl SourceWriteBatch<'_> {
    /// Insert or update one index-only file without creating sample metadata.
    pub fn upsert_source_index_entry(
        &mut self,
        entry: &SourceIndexEntry,
    ) -> Result<(), SourceDbError> {
        validate_entry(entry)?;
        let (path, path_encoding) = normalize_source_index_path(&entry.relative_path)?;
        let live_manifest_row = if path_encoding == INDEX_PATH_ENCODING_PLAIN {
            self.tx
                .query_row(
                    "SELECT 1 FROM wav_files WHERE path = ?1 AND missing = 0",
                    [&path],
                    |_| Ok(()),
                )
                .optional()
                .map_err(map_sql_error)?
                .is_some()
        } else {
            false
        };
        if live_manifest_row {
            return Err(SourceDbError::Unexpected);
        }
        let changed = self
            .tx
            .execute(
                "INSERT INTO source_index_entries (
                    path, path_encoding, classification, file_size, modified_ns, file_identity,
                    diagnostic, format_policy_version
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(path) DO UPDATE SET
                    path_encoding = excluded.path_encoding,
                    classification = excluded.classification,
                    file_size = excluded.file_size,
                    modified_ns = excluded.modified_ns,
                    file_identity = excluded.file_identity,
                    diagnostic = excluded.diagnostic,
                    format_policy_version = excluded.format_policy_version
                 WHERE classification IS NOT excluded.classification
                    OR file_size IS NOT excluded.file_size
                    OR modified_ns IS NOT excluded.modified_ns
                    OR file_identity IS NOT excluded.file_identity
                    OR diagnostic IS NOT excluded.diagnostic
                    OR format_policy_version IS NOT excluded.format_policy_version",
                params![
                    path,
                    path_encoding,
                    entry.classification.token(),
                    entry.file_size.map(saturating_i64),
                    entry.modified_ns,
                    entry.file_identity,
                    entry.diagnostic.map(SourceIndexDiagnostic::token),
                    i64::from(entry.format_policy_version),
                ],
            )
            .map_err(map_sql_error)?;
        if changed > 0 {
            self.index_revision_dirty = true;
            self.source_index_changes
                .insert(entry.relative_path.clone(), Some(entry.clone()));
        }
        Ok(())
    }

    /// Remove one index-only row, including during promotion to the supported manifest.
    pub fn remove_source_index_entry(&mut self, relative_path: &Path) -> Result<(), SourceDbError> {
        let (path, _path_encoding) = normalize_source_index_path(relative_path)?;
        let changed = self
            .tx
            .execute("DELETE FROM source_index_entries WHERE path = ?1", [path])
            .map_err(map_sql_error)?;
        if changed > 0 {
            self.index_revision_dirty = true;
            self.source_index_changes
                .insert(relative_path.to_path_buf(), None);
        }
        Ok(())
    }
}

impl SourceDatabase {
    /// Read a bounded set of index-only entries at one exact committed index revision.
    ///
    /// The revision and rows are read from one transaction. This is intended for targeted
    /// projection hydration and never materializes the source-wide index.
    pub fn source_index_entries_for_paths_at_revision(
        &self,
        expected_revision: u64,
        paths: &[std::path::PathBuf],
    ) -> Result<SourceIndexSnapshot, SourceDbError> {
        if !source_index_schema_available(&self.connection)? {
            if expected_revision != 0 {
                return Err(SourceDbError::Unexpected);
            }
            return Ok(SourceIndexSnapshot {
                revision: 0,
                entries: Vec::new(),
            });
        }
        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(map_sql_error)?;
        let revision = read_index_revision(&transaction)?;
        if revision != expected_revision {
            return Err(SourceDbError::Unexpected);
        }
        let path_encoding_available = source_index_path_encoding_available(&transaction)?;
        let mut entries = Vec::new();
        for relative_path in paths {
            let (normalized, path_encoding) = normalize_source_index_path(relative_path)?;
            if !path_encoding_available && path_encoding != 0 {
                return Err(SourceDbError::NonUnicodeRelativePath(relative_path.clone()));
            }
            let (sql, params) = if path_encoding_available {
                (
                    source_index_select_sql_for_exact_path(true),
                    ExactIndexPathParams::Encoded(normalized, path_encoding),
                )
            } else {
                (
                    source_index_select_sql_for_exact_path(false),
                    ExactIndexPathParams::Plain(normalized),
                )
            };
            entries.extend(match params {
                ExactIndexPathParams::Encoded(path, encoding) => {
                    collect_entries(&transaction, &sql, rusqlite::params![path, encoding])?
                }
                ExactIndexPathParams::Plain(path) => {
                    collect_entries(&transaction, &sql, rusqlite::params![path])?
                }
            });
        }
        entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        entries.dedup_by(|left, right| left.relative_path == right.relative_path);
        transaction.rollback().map_err(map_sql_error)?;
        Ok(SourceIndexSnapshot { revision, entries })
    }

    pub(super) fn reread_inspection_entry(
        &self,
        handle: &super::SourceIndexEntryHandle,
    ) -> Result<SourceIndexEntry, super::SourceIndexInspectionError> {
        use super::SourceIndexInspectionError as E;
        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(|_| E::Database)?;
        if !source_index_schema_available(&transaction).map_err(|_| E::Database)? {
            return Err(E::Unavailable);
        }
        if read_index_revision(&transaction).map_err(|_| E::Database)? != handle.revision() {
            return Err(E::StaleRevision);
        }
        let (key, encoding) = normalize_source_index_path(&handle.entry().relative_path)
            .map_err(|_| E::InvalidPath)?;
        let has_encoding =
            source_index_path_encoding_available(&transaction).map_err(|_| E::Database)?;
        if !has_encoding && encoding != INDEX_PATH_ENCODING_PLAIN {
            return Err(E::Unavailable);
        }
        let encoding_column = if has_encoding {
            "path_encoding"
        } else {
            "0 AS path_encoding"
        };
        let predicate = if has_encoding {
            " AND path_encoding = ?2"
        } else {
            " AND ?2 = 0"
        };
        let sql = format!(
            "SELECT path, {encoding_column}, classification, file_size, modified_ns, file_identity, diagnostic, format_policy_version FROM source_index_entries WHERE path = ?1 COLLATE BINARY{predicate} LIMIT 1"
        );
        let mut statement = transaction.prepare(&sql).map_err(|_| E::Database)?;
        let mut rows = statement
            .query(params![key, encoding])
            .map_err(|_| E::Database)?;
        let row = rows
            .next()
            .map_err(|_| E::Database)?
            .ok_or(E::ChangedEntry)?;
        let mut bytes = 0usize;
        for column in [0, 2, 5, 6] {
            bytes = bytes.saturating_add(match row.get_ref(column).map_err(|_| E::Database)? {
                rusqlite::types::ValueRef::Text(v) | rusqlite::types::ValueRef::Blob(v) => v.len(),
                _ => 0,
            });
        }
        if bytes > SOURCE_INDEX_PAGE_MAX_BYTES {
            return Err(E::ByteLimit);
        }
        let entry = strict_page_entry(row).map_err(|_| E::Database)?;
        if &entry != handle.entry() {
            return Err(E::ChangedEntry);
        }
        Ok(entry)
    }

    /// Read a bounded set of index-only entries at one exact typed path set.
    ///
    /// Each query is equality-only and the validated typed paths are never
    /// widened into subtree predicates.
    pub fn source_index_entries_for_exact_paths_at_revision(
        &self,
        expected_revision: u64,
        paths: &[RootRelativePath],
    ) -> Result<SourceIndexSnapshot, SourceDbError> {
        if !source_index_schema_available(&self.connection)? {
            if expected_revision != 0 {
                return Err(SourceDbError::Unexpected);
            }
            return Ok(SourceIndexSnapshot {
                revision: 0,
                entries: Vec::new(),
            });
        }
        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(map_sql_error)?;
        let revision = read_index_revision(&transaction)?;
        if revision != expected_revision {
            return Err(SourceDbError::Unexpected);
        }
        let path_encoding_available = source_index_path_encoding_available(&transaction)?;
        let normalized = paths
            .iter()
            .map(|path| normalize_source_index_path(path.as_path()))
            .collect::<Result<Vec<_>, _>>()?;
        const PATHS_PER_QUERY: usize = 128;
        let mut entries = Vec::new();
        for chunk in normalized.chunks(PATHS_PER_QUERY) {
            let mut parameters = Vec::with_capacity(chunk.len() * 2);
            let predicates = chunk
                .iter()
                .enumerate()
                .map(|(index, (path, encoding))| {
                    if path_encoding_available {
                        let path_parameter = index * 2 + 1;
                        let encoding_parameter = path_parameter + 1;
                        parameters.push(Value::Text(path.clone()));
                        parameters.push(Value::Integer(*encoding));
                        Ok(format!(
                            "(path = ?{path_parameter} COLLATE BINARY AND path_encoding = ?{encoding_parameter})"
                        ))
                    } else {
                        if *encoding != INDEX_PATH_ENCODING_PLAIN {
                            return Err(SourceDbError::NonUnicodeRelativePath(PathBuf::from(path)));
                        }
                        parameters.push(Value::Text(path.clone()));
                        Ok(format!("path = ?{} COLLATE BINARY", index + 1))
                    }
                })
                .collect::<Result<Vec<_>, _>>()?;
            let sql = format!(
                "SELECT path, {}, classification, file_size, modified_ns, file_identity,
                        diagnostic, format_policy_version
                 FROM source_index_entries
                 WHERE {}
                 ORDER BY path ASC",
                if path_encoding_available {
                    "path_encoding"
                } else {
                    "0 AS path_encoding"
                },
                predicates.join(" OR ")
            );
            entries.extend(collect_entries(
                &transaction,
                &sql,
                rusqlite::params_from_iter(parameters),
            )?);
        }
        entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        entries.dedup_by(|left, right| left.relative_path == right.relative_path);
        transaction.rollback().map_err(map_sql_error)?;
        Ok(SourceIndexSnapshot { revision, entries })
    }
}

fn source_index_schema_available(connection: &Connection) -> Result<bool, SourceDbError> {
    let columns = schema::table_columns(connection, "source_index_entries")?;
    Ok(REQUIRED_COLUMNS
        .iter()
        .all(|column| columns.contains(*column)))
}

fn source_index_path_encoding_available(connection: &Connection) -> Result<bool, SourceDbError> {
    Ok(schema::table_columns(connection, "source_index_entries")?.contains("path_encoding"))
}

fn source_index_select_sql(path_encoding: bool, under_path: bool) -> String {
    let encoding = if path_encoding {
        "path_encoding"
    } else {
        "0 AS path_encoding"
    };
    let predicate = if under_path {
        format!("WHERE {EXACT_SUBTREE_PATH_PREDICATE}")
    } else {
        String::new()
    };
    format!(
        "SELECT path, {encoding}, classification, file_size, modified_ns, file_identity,
                diagnostic, format_policy_version
         FROM source_index_entries
         {predicate}
         ORDER BY path ASC"
    )
}

fn source_index_select_sql_for_exact_path(path_encoding: bool) -> String {
    let encoding = if path_encoding {
        "path_encoding"
    } else {
        "0 AS path_encoding"
    };
    let predicate = if path_encoding {
        "WHERE path = ?1 COLLATE BINARY AND path_encoding = ?2"
    } else {
        "WHERE path = ?1 COLLATE BINARY"
    };
    format!(
        "SELECT path, {encoding}, classification, file_size, modified_ns, file_identity,
                diagnostic, format_policy_version
         FROM source_index_entries
         {predicate}
         ORDER BY path ASC"
    )
}

enum ExactIndexPathParams {
    Encoded(String, i64),
    Plain(String),
}

fn read_index_revision(connection: &Connection) -> Result<u64, SourceDbError> {
    connection
        .query_row(
            "SELECT value FROM metadata WHERE key = ?1",
            [META_SOURCE_INDEX_REVISION],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(map_sql_error)?
        .map(|raw| raw.parse::<u64>().map_err(|_| SourceDbError::Unexpected))
        .transpose()
        .map(Option::unwrap_or_default)
}

fn collect_entries(
    connection: &Connection,
    sql: &str,
    query_params: impl rusqlite::Params,
) -> Result<Vec<SourceIndexEntry>, SourceDbError> {
    let mut statement = connection.prepare(sql).map_err(map_sql_error)?;
    let rows = statement
        .query_map(query_params, |row| {
            let raw_path: String = row.get(0)?;
            let path_encoding: i64 = row.get(1)?;
            let relative_path = match parse_source_index_path_from_db(&raw_path, path_encoding) {
                Ok(path) => path,
                Err(error) => {
                    tracing::warn!(
                        path = raw_path,
                        %error,
                        "Skipping source index row with invalid relative path"
                    );
                    return Ok(None);
                }
            };
            let raw_classification: String = row.get(2)?;
            let Some(classification) = SourceIndexClassification::from_token(&raw_classification)
            else {
                tracing::warn!(
                    classification = raw_classification,
                    "Skipping source index row with invalid classification"
                );
                return Ok(None);
            };
            let diagnostic = row
                .get::<_, Option<String>>(6)?
                .as_deref()
                .and_then(SourceIndexDiagnostic::from_token);
            Ok(Some(SourceIndexEntry {
                relative_path,
                classification,
                file_size: row
                    .get::<_, Option<i64>>(3)?
                    .map(|value| value.max(0) as u64),
                modified_ns: row.get(4)?,
                file_identity: row.get(5)?,
                diagnostic,
                format_policy_version: row.get::<_, i64>(7)?.clamp(0, i64::from(u32::MAX)) as u32,
            }))
        })
        .map_err(map_sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_sql_error)?;
    Ok(rows.into_iter().flatten().collect())
}

fn strict_page_entry(row: &rusqlite::Row<'_>) -> Result<SourceIndexEntry, SourceDbError> {
    let raw_path = row.get::<_, String>(0).map_err(map_sql_error)?;
    let encoding = row.get::<_, i64>(1).map_err(map_sql_error)?;
    let relative_path = parse_source_index_path_from_db(&raw_path, encoding)?;
    let raw_classification = row.get::<_, String>(2).map_err(map_sql_error)?;
    let classification = SourceIndexClassification::from_token(&raw_classification)
        .ok_or(SourceDbError::Unexpected)?;
    let raw_diagnostic = row.get::<_, Option<String>>(6).map_err(map_sql_error)?;
    let diagnostic = raw_diagnostic
        .as_deref()
        .map(|token| SourceIndexDiagnostic::from_token(token).ok_or(SourceDbError::Unexpected))
        .transpose()?;
    let size = row.get::<_, Option<i64>>(3).map_err(map_sql_error)?;
    let file_size = size
        .map(|value| u64::try_from(value).map_err(|_| SourceDbError::Unexpected))
        .transpose()?;
    let version = row.get::<_, i64>(7).map_err(map_sql_error)?;
    let entry = SourceIndexEntry {
        relative_path,
        classification,
        file_size,
        modified_ns: row.get(4).map_err(map_sql_error)?,
        file_identity: row.get(5).map_err(map_sql_error)?,
        diagnostic,
        format_policy_version: u32::try_from(version).map_err(|_| SourceDbError::Unexpected)?,
    };
    validate_entry(&entry)?;
    Ok(entry)
}

fn validate_entry(entry: &SourceIndexEntry) -> Result<(), SourceDbError> {
    let complete_facts = entry.file_size.is_some() && entry.modified_ns.is_some();
    let valid = match entry.classification {
        SourceIndexClassification::UnsupportedAudio
        | SourceIndexClassification::UnsupportedNonAudio => {
            complete_facts && entry.diagnostic.is_none()
        }
        SourceIndexClassification::Inaccessible => entry.diagnostic.is_some(),
        SourceIndexClassification::PracticallyUnsupportedAudio => {
            complete_facts && entry.diagnostic == Some(SourceIndexDiagnostic::PracticalSupportLimit)
        }
    };
    if valid {
        Ok(())
    } else {
        Err(SourceDbError::Unexpected)
    }
}

fn saturating_i64(value: u64) -> i64 {
    value.min(i64::MAX as u64) as i64
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::sample_sources::SOURCE_FORMAT_POLICY_VERSION;

    fn page_entry(path: PathBuf) -> SourceIndexEntry {
        SourceIndexEntry {
            relative_path: path,
            classification: SourceIndexClassification::UnsupportedNonAudio,
            file_size: Some(5),
            modified_ns: Some(10),
            file_identity: None,
            diagnostic: None,
            format_policy_version: SOURCE_FORMAT_POLICY_VERSION,
        }
    }

    fn page_fixture(entries: &[SourceIndexEntry]) -> (tempfile::TempDir, SourceDatabase) {
        let directory = tempfile::tempdir().expect("source root");
        let database =
            SourceDatabase::open_for_source_write(directory.path()).expect("fixture writer");
        let mut batch = database.write_batch().expect("batch");
        for entry in entries {
            batch
                .upsert_source_index_entry(entry)
                .expect("index fixture");
        }
        batch.commit_auxiliary_state().expect("commit index");
        (directory, database)
    }

    #[test]
    fn index_inspection_reread_rejects_changed_revision_rows_and_oversized_fields() {
        use super::super::SourceIndexInspectionError as E;
        let (_root, database) = page_fixture(&[page_entry(PathBuf::from("a.txt"))]);
        let page = database.source_index_page(None, 1).unwrap();
        let handle = &page.inspection_handles[0];
        assert_eq!(
            database.reread_inspection_entry(handle).unwrap(),
            page.entries[0]
        );
        database
            .connection
            .execute("UPDATE source_index_entries SET file_size = 20", [])
            .unwrap();
        assert_eq!(
            database.reread_inspection_entry(handle).unwrap_err(),
            E::ChangedEntry
        );
        database
            .connection
            .execute(
                "UPDATE source_index_entries SET file_identity = ?1",
                params!["x".repeat(SOURCE_INDEX_PAGE_MAX_BYTES + 1)],
            )
            .unwrap();
        assert_eq!(
            database.reread_inspection_entry(handle).unwrap_err(),
            E::ByteLimit
        );
        database
            .connection
            .execute(
                "UPDATE metadata SET value = ?1 WHERE key = ?2",
                params![(page.revision + 1).to_string(), META_SOURCE_INDEX_REVISION],
            )
            .unwrap();
        assert_eq!(
            database.reread_inspection_entry(handle).unwrap_err(),
            E::StaleRevision
        );
        let (_other_root, other) = page_fixture(&[page_entry(PathBuf::from("a.txt"))]);
        assert_eq!(
            other.inspect_index_entry(handle).unwrap_err(),
            E::ForeignHandle
        );
        drop(database);
        let reopened = SourceDatabase::open_for_source_write(_root.path()).unwrap();
        assert_eq!(
            reopened.inspect_index_entry(handle).unwrap_err(),
            E::ForeignHandle
        );
    }

    #[test]
    fn index_page_bounds_variable_fields_before_allocation_including_lookahead() {
        let (_root, database) = page_fixture(&[
            page_entry(PathBuf::from("a.txt")),
            page_entry(PathBuf::from("b.txt")),
        ]);
        // The second row is lookahead, not returned. It must still be bounded.
        database
            .connection
            .execute(
                "UPDATE source_index_entries SET file_identity = ?1 WHERE path = 'b.txt'",
                params!["x".repeat(SOURCE_INDEX_PAGE_MAX_BYTES + 1)],
            )
            .unwrap();
        assert!(matches!(
            database.source_index_page(None, 1),
            Err(SourceIndexPageError::ByteLimit)
        ));
        database
            .connection
            .execute("UPDATE source_index_entries SET file_identity = NULL", [])
            .unwrap();
        assert_eq!(
            database.source_index_page(None, 2).unwrap().entries.len(),
            2
        );
    }

    #[test]
    fn index_page_traversal_is_bounded_ordered_and_unicode_lossless() {
        let mut entries: Vec<_> = (0..SOURCE_INDEX_PAGE_MAX_ENTRIES + 5)
            .map(|index| page_entry(PathBuf::from(format!("file-{index:04}.txt"))))
            .collect();
        entries.push(page_entry(PathBuf::from("échantillons/日本語.txt")));
        let (_directory, database) = page_fixture(&entries);
        let expected = database.source_index_snapshot().expect("oracle");
        let mut cursor = None;
        let mut collected = Vec::new();
        loop {
            let page = database
                .source_index_page(cursor.as_ref(), SOURCE_INDEX_PAGE_MAX_ENTRIES)
                .expect("bounded page");
            assert!(page.schema_available);
            assert_eq!(page.revision, expected.revision);
            assert!(page.entries.len() <= SOURCE_INDEX_PAGE_MAX_ENTRIES);
            assert!(!page.entries.is_empty());
            collected.extend(page.entries);
            cursor = page.next;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(collected, expected.entries);
        assert!(
            collected
                .iter()
                .any(|entry| entry.relative_path == Path::new("échantillons/日本語.txt"))
        );
        // A read projection never changes either durable revision.
        assert_eq!(
            database.source_index_snapshot().unwrap().revision,
            expected.revision
        );
        assert_eq!(database.get_revision().unwrap(), 0);
    }

    #[test]
    fn index_page_rejects_stale_foreign_and_reopened_cursors() {
        let (directory, database) =
            page_fixture(&[page_entry("a.txt".into()), page_entry("b.txt".into())]);
        let cursor = database.source_index_page(None, 1).unwrap().next.unwrap();
        let foreign_root = tempfile::tempdir().unwrap();
        let foreign = SourceDatabase::open_for_source_write(foreign_root.path()).unwrap();
        assert!(matches!(
            foreign.source_index_page(Some(&cursor), 1),
            Err(SourceIndexPageError::ForeignCursor)
        ));
        let mut batch = database.write_batch().unwrap();
        batch
            .upsert_source_index_entry(&page_entry("c.txt".into()))
            .unwrap();
        batch.commit_auxiliary_state().unwrap();
        assert!(matches!(
            database.source_index_page(Some(&cursor), 1),
            Err(SourceIndexPageError::StaleCursor {
                expected: 1,
                actual: 2
            })
        ));
        let current = database.source_index_page(None, 1).unwrap().next.unwrap();
        database.close().unwrap();
        let reopened = SourceDatabase::open_for_ui_read(directory.path()).unwrap();
        assert!(matches!(
            reopened.source_index_page(Some(&current), 1),
            Err(SourceIndexPageError::ForeignCursor)
        ));
        assert_eq!(
            reopened.source_index_page(None, 2).unwrap().entries.len(),
            2
        );
    }

    #[test]
    fn index_page_invalid_limits_and_malformed_rows_never_imply_empty_success() {
        let (_directory, database) =
            page_fixture(&[page_entry("a.txt".into()), page_entry("b.txt".into())]);
        for limit in [0, SOURCE_INDEX_PAGE_MAX_ENTRIES + 1, usize::MAX] {
            assert!(matches!(
                database.source_index_page(None, limit),
                Err(SourceIndexPageError::InvalidLimit)
            ));
        }
        for update in [
            "path = '../escape'",
            "path_encoding = 999",
            "classification = 'future_unknown'",
            "diagnostic = 'future_unknown'",
            "file_size = -1",
            "format_policy_version = -1",
        ] {
            let (_directory, corrupted) =
                page_fixture(&[page_entry("a.txt".into()), page_entry("b.txt".into())]);
            corrupted
                .connection
                .execute_batch("PRAGMA ignore_check_constraints = ON")
                .unwrap();
            corrupted
                .connection
                .execute(
                    &format!("UPDATE source_index_entries SET {update} WHERE path = 'b.txt'"),
                    [],
                )
                .unwrap();
            assert!(
                matches!(
                    corrupted.source_index_page(None, 1),
                    Err(SourceIndexPageError::Database(_))
                ),
                "accepted malformed row: {update}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn index_page_preserves_raw_path_and_inaccessible_diagnostic() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let raw = PathBuf::from(OsString::from_vec(b"raw-\xFF.bin".to_vec()));
        let mut entry = page_entry(raw.clone());
        entry.classification = SourceIndexClassification::Inaccessible;
        entry.diagnostic = Some(SourceIndexDiagnostic::NonUnicodePath);
        let (_directory, database) = page_fixture(&[entry.clone(), page_entry("z.txt".into())]);
        let first = database.source_index_page(None, 1).unwrap();
        let second = database.source_index_page(first.next.as_ref(), 1).unwrap();
        let entries: Vec<_> = first.entries.into_iter().chain(second.entries).collect();
        assert!(entries.contains(&entry));
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|entry| entry.relative_path == raw));
    }

    #[test]
    fn index_only_writes_advance_only_the_index_revision() {
        let directory = tempfile::tempdir().expect("source root");
        let database =
            SourceDatabase::open_for_source_write(directory.path()).expect("source database");
        let manifest_revision = database.get_revision().expect("manifest revision");
        let entry = SourceIndexEntry {
            relative_path: PathBuf::from("notes.txt"),
            classification: SourceIndexClassification::UnsupportedNonAudio,
            file_size: Some(5),
            modified_ns: Some(10),
            file_identity: None,
            diagnostic: None,
            format_policy_version: SOURCE_FORMAT_POLICY_VERSION,
        };
        let mut batch = database.write_batch().expect("index batch");
        batch
            .upsert_source_index_entry(&entry)
            .expect("upsert index entry");
        batch
            .commit_auxiliary_state()
            .expect("commit index-only state");

        let snapshot = database.source_index_snapshot().expect("index snapshot");
        assert_eq!(snapshot.revision, 1);
        assert_eq!(snapshot.entries, vec![entry]);
        assert_eq!(
            database.get_revision().expect("manifest revision"),
            manifest_revision
        );
    }

    #[test]
    fn bounded_source_commit_reports_index_facts_at_its_source_revision() {
        let directory = tempfile::tempdir().expect("source root");
        let database =
            SourceDatabase::open_for_source_write(directory.path()).expect("source database");
        let expected_revision = database.get_revision().expect("source revision");
        let entry = SourceIndexEntry {
            relative_path: PathBuf::from("notes.txt"),
            classification: SourceIndexClassification::UnsupportedNonAudio,
            file_size: Some(5),
            modified_ns: Some(10),
            file_identity: None,
            diagnostic: None,
            format_policy_version: SOURCE_FORMAT_POLICY_VERSION,
        };
        let mut batch = database.write_batch().expect("index batch");
        batch
            .upsert_source_index_entry(&entry)
            .expect("upsert index entry");
        let result = batch
            .commit_with_bounded_manifest_changes(expected_revision)
            .expect("bounded source commit");
        let index_commit = result
            .source_index_commit
            .expect("same-transaction index evidence");

        assert_eq!(index_commit.source_revision, result.revision);
        assert_eq!(index_commit.index_revision, 1);
        assert_eq!(index_commit.upserted_entries, vec![entry]);
        assert!(index_commit.removed_paths.is_empty());
        assert_eq!(result.revision, database.get_revision().unwrap());
    }

    #[test]
    fn live_manifest_and_index_only_rows_cannot_share_a_path() {
        let directory = tempfile::tempdir().expect("source root");
        let database =
            SourceDatabase::open_for_source_write(directory.path()).expect("source database");
        database
            .upsert_file(Path::new("sample.wav"), 5, 10)
            .expect("supported row");
        let entry = SourceIndexEntry {
            relative_path: PathBuf::from("sample.wav"),
            classification: SourceIndexClassification::Inaccessible,
            file_size: None,
            modified_ns: None,
            file_identity: None,
            diagnostic: Some(SourceIndexDiagnostic::OpenUnavailable),
            format_policy_version: SOURCE_FORMAT_POLICY_VERSION,
        };
        let mut batch = database.write_batch().expect("index batch");
        assert!(matches!(
            batch.upsert_source_index_entry(&entry),
            Err(SourceDbError::Unexpected)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn lossless_index_key_can_coexist_with_an_equal_manifest_storage_key() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let directory = tempfile::tempdir().expect("source root");
        let database =
            SourceDatabase::open_for_source_write(directory.path()).expect("source database");
        let raw_path = PathBuf::from_iter([
            OsString::from_vec(b"raw-\xFF".to_vec()),
            OsString::from("sample.wav"),
        ]);
        let (encoded_path, path_encoding) = normalize_source_index_path(&raw_path).unwrap();
        assert_ne!(path_encoding, INDEX_PATH_ENCODING_PLAIN);
        database
            .upsert_file(Path::new(&encoded_path), 5, 10)
            .expect("supported Unicode row with colliding storage key");

        let entry = SourceIndexEntry {
            relative_path: raw_path,
            classification: SourceIndexClassification::Inaccessible,
            file_size: Some(3),
            modified_ns: Some(20),
            file_identity: None,
            diagnostic: Some(SourceIndexDiagnostic::NonUnicodePath),
            format_policy_version: SOURCE_FORMAT_POLICY_VERSION,
        };
        let mut batch = database.write_batch().expect("index batch");
        batch
            .upsert_source_index_entry(&entry)
            .expect("lossless key must not alias a plain manifest path");
        batch.commit_auxiliary_state().expect("commit index row");

        assert_eq!(database.list_source_index_entries().unwrap(), vec![entry]);
        assert!(
            database
                .entry_for_path(Path::new(&encoded_path))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn practical_support_and_inaccessible_diagnostics_round_trip() {
        let directory = tempfile::tempdir().expect("source root");
        let database =
            SourceDatabase::open_for_source_write(directory.path()).expect("source database");
        let entries = [
            SourceIndexEntry {
                relative_path: PathBuf::from("too-long.wav"),
                classification: SourceIndexClassification::PracticallyUnsupportedAudio,
                file_size: Some(1_000),
                modified_ns: Some(20),
                file_identity: Some(String::from("file-1")),
                diagnostic: Some(SourceIndexDiagnostic::PracticalSupportLimit),
                format_policy_version: SOURCE_FORMAT_POLICY_VERSION,
            },
            SourceIndexEntry {
                relative_path: PathBuf::from("unknown.bin"),
                classification: SourceIndexClassification::Inaccessible,
                file_size: None,
                modified_ns: None,
                file_identity: None,
                diagnostic: Some(SourceIndexDiagnostic::EntryTypeUnavailable),
                format_policy_version: SOURCE_FORMAT_POLICY_VERSION,
            },
        ];
        let mut batch = database.write_batch().expect("index batch");
        for entry in &entries {
            batch
                .upsert_source_index_entry(entry)
                .expect("upsert index entry");
        }
        batch.commit_auxiliary_state().expect("commit index rows");

        assert_eq!(
            database
                .list_source_index_entries()
                .expect("read index rows"),
            entries
        );
    }

    #[test]
    fn subtree_reads_treat_sql_wildcards_as_literal_path_characters() {
        let directory = tempfile::tempdir().expect("source root");
        let database =
            SourceDatabase::open_for_source_write(directory.path()).expect("source database");
        let entries = [
            "literal%_!/inside.txt",
            "literal%_!/nested/deep.txt",
            "literalx_Yx!/outside.txt",
            "Literal%_!/case-distinct.txt",
        ]
        .map(|path| SourceIndexEntry {
            relative_path: PathBuf::from(path),
            classification: SourceIndexClassification::UnsupportedNonAudio,
            file_size: Some(5),
            modified_ns: Some(10),
            file_identity: None,
            diagnostic: None,
            format_policy_version: SOURCE_FORMAT_POLICY_VERSION,
        });
        let mut batch = database.write_batch().expect("index batch");
        for entry in &entries {
            batch
                .upsert_source_index_entry(entry)
                .expect("upsert index entry");
        }
        batch.commit_auxiliary_state().expect("commit index rows");

        assert_eq!(
            database
                .list_source_index_entries_under_path(Path::new("literal%_!"))
                .expect("read literal subtree"),
            vec![entries[0].clone(), entries[1].clone()]
        );
    }

    #[test]
    fn typed_exact_index_reads_exclude_descendants_and_siblings() {
        let directory = tempfile::tempdir().expect("source root");
        let database =
            SourceDatabase::open_for_source_write(directory.path()).expect("source database");
        let entries = [
            SourceIndexEntry {
                relative_path: PathBuf::from("target/inside.txt"),
                classification: SourceIndexClassification::UnsupportedNonAudio,
                file_size: Some(5),
                modified_ns: Some(10),
                file_identity: None,
                diagnostic: None,
                format_policy_version: SOURCE_FORMAT_POLICY_VERSION,
            },
            SourceIndexEntry {
                relative_path: PathBuf::from("target/inside/deep.txt"),
                classification: SourceIndexClassification::UnsupportedNonAudio,
                file_size: Some(6),
                modified_ns: Some(11),
                file_identity: None,
                diagnostic: None,
                format_policy_version: SOURCE_FORMAT_POLICY_VERSION,
            },
            SourceIndexEntry {
                relative_path: PathBuf::from("target/other.txt"),
                classification: SourceIndexClassification::UnsupportedNonAudio,
                file_size: Some(7),
                modified_ns: Some(12),
                file_identity: None,
                diagnostic: None,
                format_policy_version: SOURCE_FORMAT_POLICY_VERSION,
            },
        ];
        let mut batch = database.write_batch().expect("index batch");
        for entry in &entries {
            batch
                .upsert_source_index_entry(entry)
                .expect("upsert index entry");
        }
        batch.commit_auxiliary_state().expect("commit index rows");

        let exact = crate::sample_sources::reconciliation::RootRelativePath::try_from_path(
            PathBuf::from("target/inside.txt"),
        )
        .expect("valid exact path");
        let snapshot = database
            .source_index_entries_for_exact_paths(std::slice::from_ref(&exact))
            .expect("exact index snapshot");

        assert_eq!(snapshot.revision, 1);
        assert_eq!(snapshot.entries, vec![entries[0].clone()]);
    }

    #[cfg(unix)]
    #[test]
    fn typed_exact_index_reads_round_trip_non_unicode_paths() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let directory = tempfile::tempdir().expect("source root");
        let database =
            SourceDatabase::open_for_source_write(directory.path()).expect("source database");
        let relative_path = PathBuf::from(OsString::from_vec(b"raw-\xFF.txt".to_vec()));
        let entry = SourceIndexEntry {
            relative_path: relative_path.clone(),
            classification: SourceIndexClassification::UnsupportedNonAudio,
            file_size: Some(5),
            modified_ns: Some(10),
            file_identity: Some(String::from("raw-file")),
            diagnostic: None,
            format_policy_version: SOURCE_FORMAT_POLICY_VERSION,
        };
        let mut batch = database.write_batch().expect("index batch");
        batch
            .upsert_source_index_entry(&entry)
            .expect("upsert raw index entry");
        batch.commit_auxiliary_state().expect("commit index row");

        let exact =
            crate::sample_sources::reconciliation::RootRelativePath::try_from_path(relative_path)
                .expect("valid raw exact path");
        let snapshot = database
            .source_index_entries_for_exact_paths(std::slice::from_ref(&exact))
            .expect("exact raw index snapshot");

        assert_eq!(snapshot.entries, vec![entry]);
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_index_paths_and_diagnostics_round_trip_without_aliasing() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let directory = tempfile::tempdir().expect("source root");
        let database =
            SourceDatabase::open_for_source_write(directory.path()).expect("source database");
        let path = PathBuf::from_iter([
            OsString::from("folder"),
            OsString::from_vec(b"raw-\xFF.wav".to_vec()),
        ]);
        let entry = SourceIndexEntry {
            relative_path: path.clone(),
            classification: SourceIndexClassification::Inaccessible,
            file_size: Some(3),
            modified_ns: Some(10),
            file_identity: Some(String::from("file-raw")),
            diagnostic: Some(SourceIndexDiagnostic::NonUnicodePath),
            format_policy_version: SOURCE_FORMAT_POLICY_VERSION,
        };
        let mut batch = database.write_batch().expect("index batch");
        batch
            .upsert_source_index_entry(&entry)
            .expect("upsert raw index entry");
        batch.commit_auxiliary_state().expect("commit index row");

        assert_eq!(
            database.list_source_index_entries().unwrap(),
            vec![entry.clone()]
        );
        assert_eq!(
            database
                .list_source_index_entries_under_path(Path::new("folder"))
                .unwrap(),
            vec![entry]
        );
    }

    #[test]
    fn legacy_reserved_prefix_rows_rekey_before_update_and_delete() {
        let directory = tempfile::tempdir().expect("source root");
        let relative_path = PathBuf::from("~yield-nu~ff.wav");
        {
            let database =
                SourceDatabase::open_for_source_write(directory.path()).expect("source database");
            database
                .connection
                .execute(
                    "INSERT INTO source_index_entries (
                        path, path_encoding, classification, file_size, modified_ns,
                        diagnostic, format_policy_version
                     ) VALUES (?1, 0, 'unsupported_non_audio', 5, 10, NULL, ?2)",
                    params![
                        relative_path.to_string_lossy(),
                        i64::from(SOURCE_FORMAT_POLICY_VERSION)
                    ],
                )
                .expect("legacy index row");
        }

        let database =
            SourceDatabase::open_for_source_write(directory.path()).expect("reopened database");
        let updated = SourceIndexEntry {
            relative_path: relative_path.clone(),
            classification: SourceIndexClassification::UnsupportedNonAudio,
            file_size: Some(8),
            modified_ns: Some(20),
            file_identity: None,
            diagnostic: None,
            format_policy_version: SOURCE_FORMAT_POLICY_VERSION,
        };
        assert_eq!(
            database.list_source_index_entries().unwrap(),
            vec![SourceIndexEntry {
                file_size: Some(5),
                modified_ns: Some(10),
                ..updated.clone()
            }]
        );

        let mut batch = database.write_batch().expect("index batch");
        batch
            .upsert_source_index_entry(&updated)
            .expect("update rekeyed row");
        batch.commit_auxiliary_state().expect("commit update");
        assert_eq!(database.list_source_index_entries().unwrap(), vec![updated]);
        assert_eq!(
            database
                .connection
                .query_row("SELECT COUNT(*) FROM source_index_entries", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            1
        );

        let mut batch = database.write_batch().expect("index batch");
        batch
            .remove_source_index_entry(&relative_path)
            .expect("remove rekeyed row");
        batch.commit_auxiliary_state().expect("commit removal");
        assert!(database.list_source_index_entries().unwrap().is_empty());
    }
}
