//! Bounded source-tag display-label projections.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

use rusqlite::types::ValueRef;
use rusqlite::{Transaction, params};

use super::super::util::{map_sql_error, normalize_relative_path};
use super::super::{
    SourceDatabase, SourceDbError, SourceDisplayLabelEntry, SourceDisplayLabelResult,
};
use super::queries::{TagFilterProgressHandlerGuard, map_tag_filter_sql_error, source_revision};
use super::{
    PLAYBACK_TYPE_CLASSIFICATION, SOURCE_DISPLAY_LABEL_MAX_OUTPUT_BYTES,
    SOURCE_DISPLAY_LABEL_MAX_PATH_BYTES, SOURCE_DISPLAY_LABEL_MAX_PATHS,
};

const MAX_TAGS_PER_PATH: usize = 64;
const TAG_READ_PREVIEW_BYTES: usize = SOURCE_DISPLAY_LABEL_MAX_OUTPUT_BYTES + 1;

impl SourceDatabase {
    /// Read bounded metadata display labels for selected source-relative paths at one revision.
    ///
    /// Input paths are normalized and deduplicated in deterministic binary path order. Only
    /// live rows with a supported audio extension are returned; a live row with no nonempty
    /// labels is represented with `display_label: None` so callers can use the disk stem. Each
    /// path reads at most 64 tag rows plus one overflow sentinel through the path primary key.
    /// Tag labels are ordered like the native projection, and only the first recognized
    /// playback-type tag is retained. Legacy text is read through a bounded BLOB preview and
    /// sanitized to valid UTF-8. `truncated` records bounded, malformed, or control data that
    /// could not be represented completely.
    pub fn display_labels_for_paths_at_revision(
        &self,
        expected_revision: u64,
        paths: &[PathBuf],
        cancellation: &Arc<AtomicBool>,
    ) -> Result<SourceDisplayLabelResult, SourceDbError> {
        self.display_labels_for_paths_at_revision_with_progress_callback(
            expected_revision,
            paths,
            cancellation,
            None,
        )
    }

    pub(super) fn display_labels_for_paths_at_revision_with_progress_callback(
        &self,
        expected_revision: u64,
        paths: &[PathBuf],
        cancellation: &Arc<AtomicBool>,
        progress_callback: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Result<SourceDisplayLabelResult, SourceDbError> {
        if cancellation.load(Ordering::Acquire) {
            return Err(SourceDbError::Canceled);
        }
        let paths = normalize_display_label_paths(paths)?;
        if cancellation.load(Ordering::Acquire) {
            return Err(SourceDbError::Canceled);
        }

        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(map_sql_error)?;
        let revision = source_revision(&transaction)?;
        if revision != expected_revision {
            return Err(SourceDbError::DisplayLabelStaleRevision {
                expected: expected_revision,
                actual: revision,
            });
        }
        if cancellation.load(Ordering::Acquire) {
            return Err(SourceDbError::Canceled);
        }

        let entries = {
            let _progress_handler = TagFilterProgressHandlerGuard::install_with_callback(
                &self.connection,
                cancellation,
                progress_callback,
            );
            let mut entries = Vec::with_capacity(paths.len());
            for path in &paths {
                if cancellation.load(Ordering::Acquire) {
                    return Err(SourceDbError::Canceled);
                }
                if let Some(entry) = query_display_label_for_path(&transaction, path, cancellation)?
                {
                    entries.push(entry);
                }
                if cancellation.load(Ordering::Acquire) {
                    return Err(SourceDbError::Canceled);
                }
            }
            entries
        };

        if cancellation.load(Ordering::Acquire) {
            return Err(SourceDbError::Canceled);
        }
        transaction.rollback().map_err(map_sql_error)?;
        Ok(SourceDisplayLabelResult { revision, entries })
    }
}

fn query_display_label_for_path(
    transaction: &Transaction<'_>,
    path: &str,
    cancellation: &Arc<AtomicBool>,
) -> Result<Option<SourceDisplayLabelEntry>, SourceDbError> {
    let sql = display_label_rows_sql();
    let mut statement = transaction
        .prepare_cached(&sql)
        .map_err(map_tag_filter_sql_error)?;
    let mut rows = statement
        .query(params![path, (MAX_TAGS_PER_PATH + 1) as i64])
        .map_err(map_tag_filter_sql_error)?;
    let mut found_live_row = false;
    let mut tag_count = 0usize;
    let mut builder = DisplayLabelBuilder::default();
    let mut saw_playback_type = false;

    while let Some(row) = rows.next().map_err(map_tag_filter_sql_error)? {
        found_live_row = true;
        if cancellation.load(Ordering::Acquire) {
            return Err(SourceDbError::Canceled);
        }

        let Some(_tag_id) = row
            .get::<_, Option<i64>>(0)
            .map_err(map_tag_filter_sql_error)?
        else {
            continue;
        };
        if tag_count >= MAX_TAGS_PER_PATH {
            builder.truncated = true;
            break;
        }
        tag_count += 1;

        let playback_type = row
            .get::<_, Option<i64>>(3)
            .map_err(map_tag_filter_sql_error)?;
        if playback_type.is_some() {
            if saw_playback_type {
                continue;
            }
            saw_playback_type = true;
        }

        let display = row.get_ref(1).map_err(map_tag_filter_sql_error)?;
        let display_length = row
            .get::<_, Option<i64>>(2)
            .map_err(map_tag_filter_sql_error)?
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or_default();
        builder.append(display, display_length);
    }

    if !found_live_row {
        return Ok(None);
    }
    let (display_label, truncated) = builder.finish();
    Ok(Some(SourceDisplayLabelEntry {
        relative_path: PathBuf::from(path),
        display_label,
        truncated,
    }))
}

#[derive(Default)]
struct DisplayLabelBuilder {
    label: String,
    truncated: bool,
}

impl DisplayLabelBuilder {
    fn append(&mut self, value: ValueRef<'_>, raw_length: usize) {
        let bytes = match value {
            ValueRef::Blob(bytes) | ValueRef::Text(bytes) => bytes,
            ValueRef::Null => &[],
            ValueRef::Integer(_) | ValueRef::Real(_) => {
                self.truncated = true;
                return;
            }
        };
        let invalid_utf8 = std::str::from_utf8(bytes).is_err();
        if raw_length > SOURCE_DISPLAY_LABEL_MAX_OUTPUT_BYTES || raw_length > bytes.len() {
            self.truncated = true;
        }
        if invalid_utf8 {
            self.truncated = true;
        }

        let text = String::from_utf8_lossy(bytes);
        let mut wrote_tag_character = false;
        for character in text.chars() {
            if character.is_control() {
                self.truncated = true;
                continue;
            }
            let separator: usize = if self.label.is_empty() || wrote_tag_character {
                0
            } else {
                1
            };
            let required = separator.saturating_add(character.len_utf8());
            if self.label.len().saturating_add(required) > SOURCE_DISPLAY_LABEL_MAX_OUTPUT_BYTES {
                self.truncated = true;
                return;
            }
            if separator != 0 {
                self.label.push('_');
            }
            self.label.push(character);
            wrote_tag_character = true;
        }
    }

    fn finish(self) -> (Option<String>, bool) {
        (
            (!self.label.is_empty()).then_some(self.label),
            self.truncated,
        )
    }
}

fn normalize_display_label_paths(paths: &[PathBuf]) -> Result<Vec<String>, SourceDbError> {
    if paths.len() > SOURCE_DISPLAY_LABEL_MAX_PATHS {
        return Err(SourceDbError::DisplayLabelTooManyPaths {
            max: SOURCE_DISPLAY_LABEL_MAX_PATHS,
            actual: paths.len(),
        });
    }

    let raw_bytes = paths
        .iter()
        .map(|path| encoded_path_len(path))
        .fold(0usize, usize::saturating_add);
    if raw_bytes > SOURCE_DISPLAY_LABEL_MAX_PATH_BYTES {
        return Err(SourceDbError::DisplayLabelPathBytesTooLarge {
            max: SOURCE_DISPLAY_LABEL_MAX_PATH_BYTES,
            actual: raw_bytes,
        });
    }

    let normalized = paths
        .iter()
        .map(|path| normalize_relative_path(path))
        .collect::<Result<BTreeSet<_>, _>>()?
        .into_iter()
        .collect::<Vec<_>>();
    let normalized_bytes = normalized
        .iter()
        .map(String::len)
        .fold(0usize, usize::saturating_add);
    let total_bytes = raw_bytes.saturating_add(normalized_bytes);
    if total_bytes > SOURCE_DISPLAY_LABEL_MAX_PATH_BYTES {
        return Err(SourceDbError::DisplayLabelPathBytesTooLarge {
            max: SOURCE_DISPLAY_LABEL_MAX_PATH_BYTES,
            actual: total_bytes,
        });
    }
    Ok(normalized)
}

#[cfg(unix)]
fn encoded_path_len(path: &Path) -> usize {
    path.as_os_str().as_bytes().len()
}

#[cfg(not(unix))]
fn encoded_path_len(path: &Path) -> usize {
    path.to_string_lossy().len()
}

fn display_label_rows_sql() -> String {
    let supported_filter = crate::sample_sources::supported_audio_where_clause()
        .replace("extension", "wf.extension")
        .replace("path", "wf.path");
    format!(
        "SELECT st.id,
                substr(CAST(st.display_label AS BLOB), 1, {preview_bytes}),
                length(CAST(st.display_label AS BLOB)),
                ({classification})
         FROM wav_files AS wf
         LEFT JOIN wav_file_tags AS wft ON wft.path = wf.path
         LEFT JOIN source_tags AS st ON st.id = wft.tag_id
         WHERE wf.path = ?1
           AND wf.missing = 0
           AND {supported_filter}
         ORDER BY st.display_label COLLATE NOCASE ASC,
                  st.normalized_text ASC
         LIMIT ?2",
        preview_bytes = TAG_READ_PREVIEW_BYTES,
        classification = PLAYBACK_TYPE_CLASSIFICATION,
    )
}

#[cfg(test)]
pub(super) fn display_label_rows_sql_for_test() -> String {
    display_label_rows_sql()
}
