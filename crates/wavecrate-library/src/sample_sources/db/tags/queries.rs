use std::collections::{BTreeSet, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

use rusqlite::types::Type;
use rusqlite::{OptionalExtension, Params, Transaction};

use super::super::util::{map_sql_error, normalize_relative_path};
use super::super::{
    SOURCE_CURATION_MAX_TAG_LABEL_BYTES, SOURCE_TAG_PAGE_MAX_PAGE_SIZE,
    SOURCE_TAG_PAGE_MAX_PATH_BYTES, SOURCE_TAG_PAGE_MAX_PATHS, SOURCE_TAG_PAGE_MAX_PREFIX_BYTES,
    SourceDatabase, SourceDbError, SourceTag, SourceTagFilterResult, SourceTagPage,
    SourceTagPageCursor, SourceTagPageEntry, SourceTagUsage,
};
use super::identity::normalize_tag_identity;
use super::{
    SOURCE_TAG_FILTER_MAX_INPUT_BYTES, SOURCE_TAG_FILTER_MAX_TAGS,
    SOURCE_TAG_FILTER_MAX_TOKEN_BYTES,
};

const DEFAULT_TAG_LIMIT: usize = 32;
const TAG_PAGE_PREVIEW_CHAR_LIMIT: usize = 256;
const TAG_PAGE_STAGING_CHUNK_SIZE: usize = 128;
const TAG_FILTER_PROGRESS_OPS: i32 = 256;

impl SourceDatabase {
    /// Return the most-used persisted normal tags, ordered by usage then label.
    pub fn most_used_tags(&self, limit: usize) -> Result<Vec<SourceTagUsage>, SourceDbError> {
        let limit = query_limit(limit);
        let mut stmt = self
            .connection
            .prepare(
                "SELECT st.id, st.display_label, st.normalized_text, COUNT(wft.path) AS usage_count
                 FROM source_tags st
                 JOIN wav_file_tags wft ON wft.tag_id = st.id
                 GROUP BY st.id, st.display_label, st.normalized_text
                 ORDER BY usage_count DESC,
                          st.display_label COLLATE NOCASE ASC,
                          st.normalized_text ASC
                 LIMIT ?1",
            )
            .map_err(map_sql_error)?;
        collect_tag_usage(
            stmt.query_map([limit], tag_usage_from_row)
                .map_err(map_sql_error)?,
        )
    }

    /// Search persisted normal tags by normalized text. Empty input falls back to most-used tags.
    pub fn search_tags(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SourceTagUsage>, SourceDbError> {
        let identity = match normalize_tag_identity(query) {
            Ok(identity) => identity,
            Err(SourceDbError::EmptyTagLabel) => return self.most_used_tags(limit),
            Err(err) => return Err(err),
        };
        let limit = query_limit(limit);
        let mut stmt = self
            .connection
            .prepare(
                "SELECT st.id, st.display_label, st.normalized_text, COUNT(wft.path) AS usage_count
                 FROM source_tags st
                 LEFT JOIN wav_file_tags wft ON wft.tag_id = st.id
                 GROUP BY st.id, st.display_label, st.normalized_text
                 ORDER BY usage_count DESC,
                          st.display_label COLLATE NOCASE ASC,
                          st.normalized_text ASC",
            )
            .map_err(map_sql_error)?;
        let mut matches = collect_tag_usage(
            stmt.query_map([], tag_usage_from_row)
                .map_err(map_sql_error)?,
        )?
        .into_iter()
        .filter_map(|usage| {
            tag_match_rank(&usage.tag.normalized_text, &identity.normalized_text)
                .map(|rank| (rank, usage))
        })
        .collect::<Vec<_>>();

        matches.sort_by(|(left_rank, left), (right_rank, right)| {
            left_rank
                .cmp(right_rank)
                .then_with(|| right.assignment_count.cmp(&left.assignment_count))
                .then_with(|| {
                    left.tag
                        .display_label
                        .to_ascii_lowercase()
                        .cmp(&right.tag.display_label.to_ascii_lowercase())
                })
                .then_with(|| left.tag.normalized_text.cmp(&right.tag.normalized_text))
        });
        Ok(matches
            .into_iter()
            .take(limit as usize)
            .map(|(_, usage)| usage)
            .collect())
    }

    /// List normal tags assigned to one wav path.
    pub fn tags_for_path(&self, relative_path: &Path) -> Result<Vec<SourceTag>, SourceDbError> {
        let path = normalize_relative_path(relative_path)?;
        let mut stmt = self
            .connection
            .prepare(
                "SELECT st.id, st.display_label, st.normalized_text
                 FROM source_tags st
                 JOIN wav_file_tags wft ON wft.tag_id = st.id
                 WHERE wft.path = ?1
                 ORDER BY st.display_label COLLATE NOCASE ASC, st.normalized_text ASC",
            )
            .map_err(map_sql_error)?;
        let rows = stmt
            .query_map([path], |row| {
                Ok(SourceTag {
                    id: row.get(0)?,
                    display_label: row.get(1)?,
                    normalized_text: row.get(2)?,
                })
            })
            .map_err(map_sql_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sql_error)
    }

    /// List user-facing normal tag labels assigned to one wav path.
    pub fn tag_labels_for_path(&self, relative_path: &Path) -> Result<Vec<String>, SourceDbError> {
        Ok(self
            .tags_for_path(relative_path)?
            .into_iter()
            .map(|tag| tag.display_label)
            .collect())
    }

    /// Visit live supported paths carrying every required source tag at one exact revision.
    ///
    /// Filter tokens are trimmed, ASCII-lowercased, and deduplicated without changing internal
    /// punctuation. At most 64 tokens are accepted; each supplied token is limited to 1,024
    /// UTF-8 bytes and the aggregate input is limited to 64 KiB. Empty input visits every live
    /// supported path. The visitor receives a borrowed path that is valid only for its call and
    /// may return a source database error to stop the read. The cancellation flag is checked
    /// between rows and is also wired into SQLite's progress handler so a large grouped query can
    /// stop before it produces its first row.
    pub fn visit_paths_matching_tags_at_revision<S>(
        &self,
        expected_revision: u64,
        required_tags: &[S],
        cancellation: &Arc<AtomicBool>,
        mut visit: impl FnMut(&str) -> Result<(), SourceDbError>,
    ) -> Result<SourceTagFilterResult, SourceDbError>
    where
        S: AsRef<str>,
    {
        if cancellation.load(Ordering::Acquire) {
            return Err(SourceDbError::Canceled);
        }
        let required_tags = normalize_tag_filter_tokens(required_tags)?;
        if cancellation.load(Ordering::Acquire) {
            return Err(SourceDbError::Canceled);
        }

        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(map_sql_error)?;
        let revision = source_revision(&transaction)?;
        if revision != expected_revision {
            return Err(SourceDbError::Unexpected);
        }
        if cancellation.load(Ordering::Acquire) {
            return Err(SourceDbError::Canceled);
        }

        let matched_paths = if required_tags.is_empty() {
            let sql = all_supported_tag_filter_paths_sql();
            let _progress_handler =
                TagFilterProgressHandlerGuard::install(&self.connection, cancellation);
            visit_tag_filter_rows(&transaction, &sql, [], cancellation, &mut visit)?
        } else {
            stage_tag_filter_tokens(&transaction, &required_tags, cancellation)?;
            let sql = matching_tag_filter_paths_sql();
            let required_count = i64::try_from(required_tags.len()).unwrap_or(i64::MAX);
            let _progress_handler =
                TagFilterProgressHandlerGuard::install(&self.connection, cancellation);
            visit_tag_filter_rows(
                &transaction,
                &sql,
                [required_count],
                cancellation,
                &mut visit,
            )?
        };

        if cancellation.load(Ordering::Acquire) {
            return Err(SourceDbError::Canceled);
        }
        transaction.rollback().map_err(map_sql_error)?;
        Ok(SourceTagFilterResult {
            revision,
            matched_paths: matched_paths as u64,
        })
    }

    /// Fetch tags assigned to an exact bounded set of source-relative paths at one revision.
    ///
    /// Paths are normalized and deduplicated before the single read transaction. The returned
    /// assignment count is the number of distinct selected paths carrying each tag. The page
    /// size is clamped to `1..=SOURCE_TAG_PAGE_MAX_PAGE_SIZE`, matching the browser metadata page
    /// API. A cursor is valid only for the same revision and canonical selected-path set.
    pub fn tags_for_paths_page_at_revision(
        &self,
        expected_revision: u64,
        paths: &[PathBuf],
        after_cursor: Option<&SourceTagPageCursor>,
        requested_limit: usize,
    ) -> Result<SourceTagPage, SourceDbError> {
        self.tags_for_paths_page_at_revision_with_cancellation(
            expected_revision,
            paths,
            after_cursor,
            requested_limit,
            || false,
        )
    }

    /// Fetch tags for an exact bounded set of source-relative paths with cancellation checks.
    ///
    /// This is the cancellation-aware form used by the catalog worker while it stages a large
    /// selection. The callback is checked between bounded staging chunks and before the grouped
    /// read. A canceled operation rolls back the read transaction, including its temporary
    /// selection table.
    pub fn tags_for_paths_page_at_revision_with_cancellation(
        &self,
        expected_revision: u64,
        paths: &[PathBuf],
        after_cursor: Option<&SourceTagPageCursor>,
        requested_limit: usize,
        mut is_canceled: impl FnMut() -> bool,
    ) -> Result<SourceTagPage, SourceDbError> {
        let paths = normalize_tag_page_paths(paths)?;
        let query_signature = tag_page_paths_signature(&paths);
        let limit = tag_page_limit(requested_limit);
        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(map_sql_error)?;
        let revision = source_revision(&transaction)?;
        if revision != expected_revision {
            return Err(SourceDbError::Unexpected);
        }
        let cursor_id =
            validate_tag_page_cursor(&transaction, revision, query_signature, after_cursor)?;
        let mut entries = if paths.is_empty() {
            Vec::new()
        } else {
            stage_selected_paths(&transaction, &paths, &mut is_canceled)?;
            query_tags_for_paths(&transaction, cursor_id, limit)?
        };
        let has_more = entries.len() > limit;
        if has_more {
            entries.truncate(limit);
        }
        let next_cursor = has_more.then(|| SourceTagPageCursor {
            revision,
            query_signature,
            last_tag_id: entries
                .last()
                .expect("a page with continuation has a last entry")
                .id,
        });
        transaction.rollback().map_err(map_sql_error)?;
        Ok(SourceTagPage {
            revision,
            entries,
            next_cursor,
        })
    }

    /// Fetch source vocabulary tags matching a literal normalized prefix at one revision.
    ///
    /// Empty or whitespace-only prefixes return the whole vocabulary in canonical order. The
    /// page size is clamped to `1..=SOURCE_TAG_PAGE_MAX_PAGE_SIZE`, matching the browser metadata
    /// page API. Vocabulary entries omit assignment counts. A cursor is valid only for the same
    /// revision and normalized prefix.
    pub fn tag_vocabulary_page_at_revision(
        &self,
        expected_revision: u64,
        prefix: &str,
        after_cursor: Option<&SourceTagPageCursor>,
        requested_limit: usize,
    ) -> Result<SourceTagPage, SourceDbError> {
        let prefix = normalize_tag_page_prefix(prefix)?;
        let query_signature = tag_page_prefix_signature(&prefix);
        let limit = tag_page_limit(requested_limit);
        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(map_sql_error)?;
        let revision = source_revision(&transaction)?;
        if revision != expected_revision {
            return Err(SourceDbError::Unexpected);
        }
        let cursor_id =
            validate_tag_page_cursor(&transaction, revision, query_signature, after_cursor)?;
        let entries = query_tag_vocabulary(&transaction, &prefix, cursor_id, limit)?;
        let (entries, next_cursor) =
            split_tag_page_entries(entries, limit, revision, query_signature);
        transaction.rollback().map_err(map_sql_error)?;
        Ok(SourceTagPage {
            revision,
            entries,
            next_cursor,
        })
    }
}

fn normalize_tag_filter_tokens<S>(required_tags: &[S]) -> Result<Vec<String>, SourceDbError>
where
    S: AsRef<str>,
{
    if required_tags.len() > SOURCE_TAG_FILTER_MAX_TAGS {
        return Err(SourceDbError::TagFilterTooManyTokens {
            max: SOURCE_TAG_FILTER_MAX_TAGS,
            actual: required_tags.len(),
        });
    }

    let mut input_bytes = 0usize;
    let mut normalized = BTreeSet::new();
    for required_tag in required_tags {
        let raw = required_tag.as_ref();
        let raw_bytes = raw.len();
        if raw_bytes > SOURCE_TAG_FILTER_MAX_TOKEN_BYTES {
            return Err(SourceDbError::TagFilterTokenTooLong {
                max: SOURCE_TAG_FILTER_MAX_TOKEN_BYTES,
                actual: raw_bytes,
            });
        }
        input_bytes = input_bytes.saturating_add(raw_bytes);
        if input_bytes > SOURCE_TAG_FILTER_MAX_INPUT_BYTES {
            return Err(SourceDbError::TagFilterInputTooLarge {
                max: SOURCE_TAG_FILTER_MAX_INPUT_BYTES,
                actual: input_bytes,
            });
        }

        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            normalized.insert(trimmed.to_ascii_lowercase());
        }
    }
    Ok(normalized.into_iter().collect())
}

fn stage_tag_filter_tokens(
    transaction: &Transaction<'_>,
    required_tags: &[String],
    cancellation: &Arc<AtomicBool>,
) -> Result<(), SourceDbError> {
    if cancellation.load(Ordering::Acquire) {
        return Err(SourceDbError::Canceled);
    }
    transaction
        .execute_batch(
            "CREATE TEMP TABLE tag_filter_tokens (
                ordinal INTEGER PRIMARY KEY,
                normalized_text TEXT NOT NULL
            ) WITHOUT ROWID",
        )
        .map_err(map_sql_error)?;
    let mut statement = transaction
        .prepare_cached(
            "INSERT OR IGNORE INTO temp.tag_filter_tokens (ordinal, normalized_text)
             VALUES (?1, ?2)",
        )
        .map_err(map_sql_error)?;
    for (ordinal, required_tag) in required_tags.iter().enumerate() {
        if cancellation.load(Ordering::Acquire) {
            return Err(SourceDbError::Canceled);
        }
        statement
            .execute(rusqlite::params![ordinal as i64, required_tag])
            .map_err(map_sql_error)?;
    }
    drop(statement);
    if cancellation.load(Ordering::Acquire) {
        return Err(SourceDbError::Canceled);
    }
    Ok(())
}

fn all_supported_tag_filter_paths_sql() -> String {
    format!(
        "SELECT path
         FROM wav_files
         WHERE {filter} AND missing = 0
         ORDER BY path COLLATE BINARY ASC",
        filter = crate::sample_sources::supported_audio_where_clause(),
    )
}

fn matching_tag_filter_paths_sql() -> String {
    // Keep the indexed NOCASE candidate lookup, then use SQLite's ASCII lower() with a binary
    // comparison so NOCASE's embedded-NUL truncation cannot turn distinct tokens into a match.
    format!(
        "SELECT wft.path
         FROM temp.tag_filter_tokens AS required
         CROSS JOIN source_tags AS st
           ON st.normalized_text COLLATE NOCASE = required.normalized_text
          AND lower(st.normalized_text) = required.normalized_text COLLATE BINARY
         CROSS JOIN wav_file_tags AS wft
           ON wft.tag_id = st.id
         JOIN wav_files AS wf ON wf.path = wft.path
         WHERE {filter} AND wf.missing = 0
         GROUP BY wft.path
         HAVING COUNT(DISTINCT required.ordinal) = ?1
         ORDER BY wft.path COLLATE BINARY ASC",
        filter = crate::sample_sources::supported_audio_where_clause()
            .replace("path", "wf.path")
            .replace("extension", "wf.extension"),
    )
}

#[cfg(test)]
pub(super) fn matching_tag_filter_paths_sql_for_test() -> String {
    matching_tag_filter_paths_sql()
}

fn visit_tag_filter_rows(
    transaction: &Transaction<'_>,
    sql: &str,
    params: impl Params,
    cancellation: &Arc<AtomicBool>,
    visit: &mut impl FnMut(&str) -> Result<(), SourceDbError>,
) -> Result<usize, SourceDbError> {
    let mut statement = transaction
        .prepare_cached(sql)
        .map_err(map_tag_filter_sql_error)?;
    let mut rows = statement.query(params).map_err(map_tag_filter_sql_error)?;
    let mut matched_paths = 0usize;
    while let Some(row) = rows.next().map_err(map_tag_filter_sql_error)? {
        if cancellation.load(Ordering::Acquire) {
            return Err(SourceDbError::Canceled);
        }
        let path = row
            .get_ref(0)
            .map_err(map_tag_filter_sql_error)?
            .as_str()
            .map_err(|error| {
                map_tag_filter_sql_error(rusqlite::Error::FromSqlConversionFailure(
                    0,
                    Type::Text,
                    Box::new(error),
                ))
            })?;
        visit(path)?;
        matched_paths = matched_paths.saturating_add(1);
    }
    if cancellation.load(Ordering::Acquire) {
        return Err(SourceDbError::Canceled);
    }
    Ok(matched_paths)
}

fn map_tag_filter_sql_error(error: rusqlite::Error) -> SourceDbError {
    if matches!(
        &error,
        rusqlite::Error::SqliteFailure(sql_error, _)
            if sql_error.code == rusqlite::ErrorCode::OperationInterrupted
    ) {
        SourceDbError::Canceled
    } else {
        map_sql_error(error)
    }
}

struct TagFilterProgressHandlerGuard<'connection> {
    connection: &'connection rusqlite::Connection,
}

impl<'connection> TagFilterProgressHandlerGuard<'connection> {
    fn install(
        connection: &'connection rusqlite::Connection,
        cancellation: &Arc<AtomicBool>,
    ) -> Self {
        let cancellation = Arc::clone(cancellation);
        connection.progress_handler(
            TAG_FILTER_PROGRESS_OPS,
            Some(move || cancellation.load(Ordering::Acquire)),
        );
        Self { connection }
    }
}

impl Drop for TagFilterProgressHandlerGuard<'_> {
    fn drop(&mut self) {
        self.connection.progress_handler(0, None::<fn() -> bool>);
    }
}

fn query_limit(limit: usize) -> i64 {
    limit.clamp(1, DEFAULT_TAG_LIMIT) as i64
}

fn tag_usage_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SourceTagUsage> {
    let assignment_count: i64 = row.get(3)?;
    Ok(SourceTagUsage {
        tag: SourceTag {
            id: row.get(0)?,
            display_label: row.get(1)?,
            normalized_text: row.get(2)?,
        },
        assignment_count: assignment_count.max(0) as u64,
    })
}

fn collect_tag_usage(
    rows: rusqlite::MappedRows<
        '_,
        impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<SourceTagUsage>,
    >,
) -> Result<Vec<SourceTagUsage>, SourceDbError> {
    rows.collect::<Result<Vec<_>, _>>().map_err(map_sql_error)
}

fn tag_match_rank(candidate: &str, query: &str) -> Option<u8> {
    if candidate == query {
        return Some(0);
    }
    if candidate.contains(query) {
        return Some(1);
    }
    fuzzy_subsequence_match(candidate, query).then_some(2)
}

fn fuzzy_subsequence_match(candidate: &str, query: &str) -> bool {
    let mut chars = candidate.chars();
    query
        .chars()
        .all(|query_char| chars.any(|candidate_char| candidate_char == query_char))
}

fn tag_page_limit(requested_limit: usize) -> usize {
    requested_limit.clamp(1, SOURCE_TAG_PAGE_MAX_PAGE_SIZE)
}

fn normalize_tag_page_paths(paths: &[PathBuf]) -> Result<Vec<String>, SourceDbError> {
    if paths.len() > SOURCE_TAG_PAGE_MAX_PATHS {
        return Err(SourceDbError::TagPageTooManyPaths {
            max: SOURCE_TAG_PAGE_MAX_PATHS,
            actual: paths.len(),
        });
    }

    let mut path_bytes = 0usize;
    for path in paths {
        path_bytes = path_bytes.saturating_add(encoded_path_len(path));
    }
    if path_bytes > SOURCE_TAG_PAGE_MAX_PATH_BYTES {
        return Err(SourceDbError::TagPagePathBytesTooLarge {
            max: SOURCE_TAG_PAGE_MAX_PATH_BYTES,
            actual: path_bytes,
        });
    }

    paths
        .iter()
        .map(|path| normalize_relative_path(path))
        .collect::<Result<BTreeSet<_>, _>>()
        .map(|paths| paths.into_iter().collect())
}

#[cfg(unix)]
fn encoded_path_len(path: &Path) -> usize {
    path.as_os_str().as_bytes().len()
}

#[cfg(not(unix))]
fn encoded_path_len(path: &Path) -> usize {
    path.to_string_lossy().len()
}

fn normalize_tag_page_prefix(prefix: &str) -> Result<String, SourceDbError> {
    if prefix.len() > SOURCE_TAG_PAGE_MAX_PREFIX_BYTES {
        return Err(SourceDbError::TagPagePrefixTooLong {
            max: SOURCE_TAG_PAGE_MAX_PREFIX_BYTES,
            actual: prefix.len(),
        });
    }
    if prefix.contains('\0') {
        return Err(SourceDbError::TagPagePrefixContainsNul);
    }
    match normalize_tag_identity(prefix) {
        Ok(identity) => Ok(identity.normalized_text),
        Err(SourceDbError::EmptyTagLabel) => Ok(String::new()),
        Err(error) => Err(error),
    }
}

fn tag_page_paths_signature(paths: &[String]) -> u64 {
    let mut hasher = DefaultHasher::new();
    "wavecrate-source-tag-page-v1".hash(&mut hasher);
    "paths".hash(&mut hasher);
    paths.hash(&mut hasher);
    hasher.finish()
}

fn tag_page_prefix_signature(prefix: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    "wavecrate-source-tag-page-v1".hash(&mut hasher);
    "vocabulary".hash(&mut hasher);
    prefix.hash(&mut hasher);
    hasher.finish()
}

fn source_revision(transaction: &Transaction<'_>) -> Result<u64, SourceDbError> {
    transaction
        .query_row(
            "SELECT value FROM metadata WHERE key = 'revision'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(map_sql_error)?
        .map(|raw| raw.parse::<u64>().map_err(|_| SourceDbError::Unexpected))
        .transpose()
        .map(|revision| revision.unwrap_or_default())
}

fn validate_tag_page_cursor(
    transaction: &Transaction<'_>,
    revision: u64,
    query_signature: u64,
    cursor: Option<&SourceTagPageCursor>,
) -> Result<Option<i64>, SourceDbError> {
    let Some(cursor) = cursor else {
        return Ok(None);
    };
    if cursor.revision != revision || cursor.query_signature != query_signature {
        return Err(SourceDbError::Unexpected);
    }
    let exists = transaction
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM source_tags WHERE id = ?1)",
            [cursor.last_tag_id],
            |row| row.get::<_, i64>(0),
        )
        .map_err(map_sql_error)?
        != 0;
    exists
        .then_some(cursor.last_tag_id)
        .ok_or(SourceDbError::Unexpected)
        .map(Some)
}

fn bounded_tag_text_sql(column: &str) -> String {
    format!(
        "CASE WHEN length(CAST({column} AS BLOB)) <= {max_bytes}
                  AND instr({column}, char(0)) = 0
              THEN {column}
              ELSE substr({column}, 1, {preview_chars})
         END",
        max_bytes = SOURCE_CURATION_MAX_TAG_LABEL_BYTES,
        preview_chars = TAG_PAGE_PREVIEW_CHAR_LIMIT,
    )
}

fn tag_preview_sql() -> String {
    format!(
        "(
        length(CAST(st.display_label AS BLOB)) > {max_bytes}
        OR instr(st.display_label, char(0)) > 0
        OR length(CAST(st.normalized_text AS BLOB)) > {max_bytes}
        OR instr(st.normalized_text, char(0)) > 0
    )",
        max_bytes = SOURCE_CURATION_MAX_TAG_LABEL_BYTES,
    )
}

fn stage_selected_paths(
    transaction: &Transaction<'_>,
    paths: &[String],
    is_canceled: &mut impl FnMut() -> bool,
) -> Result<(), SourceDbError> {
    if is_canceled() {
        return Err(SourceDbError::Canceled);
    }
    transaction
        .execute_batch(
            "CREATE TEMP TABLE selected_paths (
                path TEXT PRIMARY KEY
            ) WITHOUT ROWID",
        )
        .map_err(map_sql_error)?;
    let mut statement = transaction
        .prepare_cached(
            "INSERT OR IGNORE INTO temp.selected_paths (path)
             VALUES (?1)",
        )
        .map_err(map_sql_error)?;
    for chunk in paths.chunks(TAG_PAGE_STAGING_CHUNK_SIZE) {
        if is_canceled() {
            return Err(SourceDbError::Canceled);
        }
        for path in chunk {
            statement.execute([path]).map_err(map_sql_error)?;
        }
        if is_canceled() {
            return Err(SourceDbError::Canceled);
        }
    }
    drop(statement);
    Ok(())
}

fn query_tags_for_paths(
    transaction: &Transaction<'_>,
    cursor_id: Option<i64>,
    limit: usize,
) -> Result<Vec<SourceTagPageEntry>, SourceDbError> {
    let sql = selected_tag_page_sql();
    let fetch_limit = i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX);
    let mut statement = transaction.prepare_cached(&sql).map_err(map_sql_error)?;
    statement
        .query_map(
            rusqlite::params![cursor_id, fetch_limit],
            tag_page_entry_with_count,
        )
        .map_err(map_sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_sql_error)
}

fn selected_tag_page_sql() -> String {
    format!(
        "WITH selected_tags AS (
                  SELECT wft.tag_id, COUNT(*) AS assignment_count
                  FROM temp.selected_paths AS selected
                  CROSS JOIN wav_file_tags AS wft
                  WHERE wft.path = selected.path
                  GROUP BY wft.tag_id
              )
         SELECT st.id,
                {display_label},
                {normalized_text},
                {preview},
                selected_tags.assignment_count
         FROM source_tags AS st
         JOIN selected_tags ON selected_tags.tag_id = st.id
         WHERE (?1 IS NULL OR (st.normalized_text, st.id) >
                (SELECT normalized_text, id FROM source_tags WHERE id = ?1))
         ORDER BY st.normalized_text COLLATE BINARY ASC, st.id ASC
         LIMIT ?2",
        display_label = bounded_tag_text_sql("st.display_label"),
        normalized_text = bounded_tag_text_sql("st.normalized_text"),
        preview = tag_preview_sql(),
    )
}

fn query_tag_vocabulary(
    transaction: &Transaction<'_>,
    prefix: &str,
    cursor_id: Option<i64>,
    limit: usize,
) -> Result<Vec<SourceTagPageEntry>, SourceDbError> {
    let fetch_limit = i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX);
    let sql = tag_vocabulary_sql(prefix.is_empty(), cursor_id.is_some());

    if prefix.is_empty() {
        if let Some(cursor_id) = cursor_id {
            let mut statement = transaction.prepare_cached(&sql).map_err(map_sql_error)?;
            return statement
                .query_map(
                    rusqlite::params![cursor_id, fetch_limit],
                    tag_page_entry_without_count,
                )
                .map_err(map_sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(map_sql_error);
        }

        let mut statement = transaction.prepare_cached(&sql).map_err(map_sql_error)?;
        return statement
            .query_map([fetch_limit], tag_page_entry_without_count)
            .map_err(map_sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sql_error);
    }

    let upper_bound = tag_prefix_upper_bound(prefix).expect("a non-empty UTF-8 prefix has a bound");
    if let Some(cursor_id) = cursor_id {
        let mut statement = transaction.prepare_cached(&sql).map_err(map_sql_error)?;
        return statement
            .query_map(
                rusqlite::params![upper_bound, prefix, cursor_id, fetch_limit],
                tag_page_entry_without_count,
            )
            .map_err(map_sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sql_error);
    }

    let mut statement = transaction.prepare_cached(&sql).map_err(map_sql_error)?;
    statement
        .query_map(
            rusqlite::params![prefix, upper_bound, fetch_limit],
            tag_page_entry_without_count,
        )
        .map_err(map_sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_sql_error)
}

fn tag_vocabulary_sql(prefix_is_empty: bool, cursor_present: bool) -> String {
    let display_label = bounded_tag_text_sql("st.display_label");
    let normalized_text = bounded_tag_text_sql("st.normalized_text");
    let preview = tag_preview_sql();
    let where_clause = match (prefix_is_empty, cursor_present) {
        (true, false) => String::new(),
        (true, true) => String::from(
            "WHERE (st.normalized_text, st.id) >
                  (SELECT normalized_text, id FROM source_tags WHERE id = ?1)",
        ),
        (false, false) => String::from(
            "WHERE st.normalized_text >= ?1 COLLATE BINARY
                AND st.normalized_text < CAST(?2 AS TEXT) COLLATE BINARY
                AND substr(st.normalized_text, 1, length(?1)) = ?1 COLLATE BINARY",
        ),
        (false, true) => String::from(
            "WHERE st.normalized_text < CAST(?1 AS TEXT) COLLATE BINARY
                AND substr(st.normalized_text, 1, length(?2)) = ?2 COLLATE BINARY
                AND (st.normalized_text, st.id) >
                    (SELECT normalized_text, id FROM source_tags WHERE id = ?3)",
        ),
    };
    let limit_parameter = match (prefix_is_empty, cursor_present) {
        (true, false) => 1,
        (true, true) => 2,
        (false, false) => 3,
        (false, true) => 4,
    };
    format!(
        "SELECT st.id,
                {display_label},
                {normalized_text},
                {preview}
         FROM source_tags AS st
         {where_clause}
         ORDER BY st.normalized_text COLLATE BINARY ASC, st.id ASC
         LIMIT ?{limit_parameter}"
    )
}

fn tag_prefix_upper_bound(prefix: &str) -> Option<Vec<u8>> {
    let mut upper_bound = prefix.as_bytes().to_vec();
    while upper_bound.last().is_some_and(|byte| *byte == u8::MAX) {
        upper_bound.pop();
    }
    let last = upper_bound.last_mut()?;
    *last = last.checked_add(1)?;
    Some(upper_bound)
}

fn tag_page_entry_with_count(row: &rusqlite::Row<'_>) -> rusqlite::Result<SourceTagPageEntry> {
    tag_page_entry_from_row(row, Some(4))
}

fn tag_page_entry_without_count(row: &rusqlite::Row<'_>) -> rusqlite::Result<SourceTagPageEntry> {
    tag_page_entry_from_row(row, None)
}

fn tag_page_entry_from_row(
    row: &rusqlite::Row<'_>,
    assignment_count_column: Option<usize>,
) -> rusqlite::Result<SourceTagPageEntry> {
    let assignment_count = assignment_count_column
        .map(|column| row.get::<_, i64>(column))
        .transpose()?
        .map(|count| count.max(0) as u64);
    Ok(SourceTagPageEntry {
        id: row.get(0)?,
        display_label: row.get(1)?,
        normalized_text: row.get(2)?,
        is_preview: row.get::<_, i64>(3)? != 0,
        assignment_count,
    })
}

fn split_tag_page_entries(
    mut entries: Vec<SourceTagPageEntry>,
    limit: usize,
    revision: u64,
    query_signature: u64,
) -> (Vec<SourceTagPageEntry>, Option<SourceTagPageCursor>) {
    let has_more = entries.len() > limit;
    if has_more {
        entries.truncate(limit);
    }
    let next_cursor = has_more.then(|| SourceTagPageCursor {
        revision,
        query_signature,
        last_tag_id: entries
            .last()
            .expect("a page with continuation has a last entry")
            .id,
    });
    (entries, next_cursor)
}

#[cfg(test)]
mod plan_tests {
    use std::path::Path;

    use tempfile::tempdir;

    use super::{
        SourceDatabase, selected_tag_page_sql, tag_prefix_upper_bound, tag_vocabulary_sql,
    };

    #[test]
    fn vocabulary_prefix_range_plans_include_an_index_seek() {
        let directory = tempdir().unwrap();
        let database = SourceDatabase::open_for_user_metadata_write(directory.path()).unwrap();
        database.upsert_file(Path::new("one.wav"), 10, 5).unwrap();
        for label in ["alpha", "atom", "beta"] {
            database
                .assign_tag_to_path(Path::new("one.wav"), label)
                .unwrap();
        }
        let upper_bound = tag_prefix_upper_bound("a").unwrap();

        let no_cursor_opcodes = explain_opcodes(
            &database,
            &format!("EXPLAIN {}", tag_vocabulary_sql(false, false)),
            rusqlite::params!["a", upper_bound.clone(), 2_i64],
        );
        assert!(no_cursor_opcodes.iter().any(|opcode| opcode == "SeekGE"));

        let cursor_opcodes = explain_opcodes(
            &database,
            &format!("EXPLAIN {}", tag_vocabulary_sql(false, true)),
            rusqlite::params![upper_bound, "a", 1_i64, 2_i64],
        );
        assert!(cursor_opcodes.iter().any(|opcode| opcode == "SeekGE"));
    }

    #[test]
    fn selected_tag_page_plan_uses_wav_file_tag_primary_key_lookup() {
        let directory = tempdir().unwrap();
        let database = SourceDatabase::open_for_user_metadata_write(directory.path()).unwrap();
        database.upsert_file(Path::new("one.wav"), 10, 5).unwrap();
        database
            .assign_tag_to_path(Path::new("one.wav"), "shared")
            .unwrap();
        let transaction = database.connection.unchecked_transaction().unwrap();
        transaction
            .execute_batch(
                "CREATE TEMP TABLE selected_paths (
                    path TEXT PRIMARY KEY
                ) WITHOUT ROWID;
                INSERT INTO temp.selected_paths (path) VALUES ('one.wav');",
            )
            .unwrap();
        let mut statement = transaction
            .prepare(&format!("EXPLAIN QUERY PLAN {}", selected_tag_page_sql()))
            .unwrap();
        let details = statement
            .query_map(rusqlite::params![Option::<i64>::None, 2_i64], |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(
            details
                .iter()
                .any(|detail| detail.contains("SEARCH wft USING PRIMARY KEY")
                    && detail.contains("path=?")),
            "selected tag query plan did not use the wav tag primary key: {details:?}"
        );
        drop(statement);
        transaction.rollback().unwrap();
    }

    fn explain_opcodes<P: rusqlite::Params>(
        database: &SourceDatabase,
        sql: &str,
        parameters: P,
    ) -> Vec<String> {
        let mut statement = database.connection.prepare(sql).unwrap();
        statement
            .query_map(parameters, |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }
}
