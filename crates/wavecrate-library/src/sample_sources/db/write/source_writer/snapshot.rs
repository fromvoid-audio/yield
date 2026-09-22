//! Bounded, non-mutating reads on the retained source-writer connection.

use std::mem::size_of;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use rusqlite::{Connection, ErrorCode, OptionalExtension, Row, types::ValueRef};

use super::*;
use crate::sample_sources::db::util::parse_relative_path_from_db;
use crate::sample_sources::db::{SourceManifestEntry, SourcePublicationFence};

// One page is strictly below the 100,000-row aggregate request budget.
const MAX_PAGE_ROWS: usize = 64;
const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;
const MAX_REQUEST_TIME: Duration = Duration::from_secs(2);
const PROGRESS_OPS: i32 = 1_000;

/// Opaque keyset cut bound to one live source identity and exact revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceManifestSnapshotCursor {
    revision: SourceRevision,
    raw_path: String,
}

/// One fresh-as-of, source-owned manifest page from a single SQLite read transaction.
///
/// The weak publication fence must be validated again by a caller immediately before
/// publishing the page. Neither this page nor its cursor grants mutation authority.
#[derive(Clone, Debug)]
pub struct SourceManifestSnapshotPage {
    /// Exact retained source revision observed by the read transaction.
    pub revision: SourceRevision,
    /// Live supported-audio manifest rows in SQLite path-key order.
    pub entries: Vec<SourceManifestEntry>,
    /// Exact continuation cut; absent when the page reaches the end.
    pub next_cursor: Option<SourceManifestSnapshotCursor>,
    /// Weak owner/epoch fence for the caller's publication boundary.
    pub publication_fence: SourcePublicationFence,
}

struct SnapshotProgressGuard<'connection>(&'connection Connection);

impl SnapshotProgressGuard<'_> {
    fn install<'connection>(
        connection: &'connection Connection,
        cancellation: &Arc<AtomicBool>,
        started: Instant,
    ) -> SnapshotProgressGuard<'connection> {
        let cancellation = Arc::clone(cancellation);
        connection.progress_handler(
            PROGRESS_OPS,
            Some(move || {
                cancellation.load(Ordering::Acquire) || started.elapsed() >= MAX_REQUEST_TIME
            }),
        );
        SnapshotProgressGuard(connection)
    }
}

impl Drop for SnapshotProgressGuard<'_> {
    fn drop(&mut self) {
        self.0.progress_handler(0, None::<fn() -> bool>);
    }
}

impl SourceWriter {
    /// Read one bounded manifest page through the already-owned source connection.
    ///
    /// Every call validates the exact source identity, lease epoch, lifecycle generation,
    /// and revision. Continuations cannot combine rows from different revisions. The read
    /// performs no schema assurance, temporary-table mutation, checkpoint, or backup.
    pub fn read_manifest_snapshot_page(
        &self,
        expected: &SourceRevision,
        after: Option<&SourceManifestSnapshotCursor>,
        requested_limit: usize,
        cancellation: &Arc<AtomicBool>,
    ) -> Result<SourceManifestSnapshotPage, SourceWriterError> {
        if !(1..=MAX_PAGE_ROWS).contains(&requested_limit) {
            return Err(SourceWriterError::InvalidRequest(
                "manifest snapshot page limit must be from 1 through 64",
            ));
        }
        if cancellation.load(Ordering::Acquire) {
            return Err(SourceDbError::Canceled.into());
        }
        let started = Instant::now();
        let profile_guard = self
            .database
            .profile_guard
            .as_ref()
            .ok_or(SourceDbError::ProfileOwnershipRequired)?;
        self.database
            .validate_operational_schema_with_profile_guard(profile_guard)?;
        let current = self.current_revision()?;
        ensure_snapshot_revision(expected, &current)?;
        if let Some(cursor) = after {
            ensure_snapshot_revision(&cursor.revision, expected)?;
        }
        check_budget(started, cancellation)?;
        let publication_fence = self.database.source_publication_fence()?;
        publication_fence
            .validate()
            .map_err(|_| SourceWriterError::StaleFence)?;
        check_budget(started, cancellation)?;

        let _progress =
            SnapshotProgressGuard::install(&self.database.connection, cancellation, started);
        let transaction = self
            .database
            .connection
            .unchecked_transaction()
            .map_err(|error| map_snapshot_sql(error, cancellation, started))?;
        let observed = transaction
            .query_row(
                "SELECT value FROM metadata WHERE key = 'revision'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| map_snapshot_sql(error, cancellation, started))?
            .ok_or(SourceWriterError::SnapshotMalformed(
                "source revision metadata is missing",
            ))?
            .parse::<u64>()
            .map_err(|_| SourceWriterError::SnapshotMalformed("source revision is malformed"))?;
        if observed != expected.revision {
            return Err(SourceWriterError::StaleRevision {
                expected: expected.revision,
                actual: observed,
            });
        }

        let revision_bytes = expected
            .source_id
            .as_str()
            .len()
            .saturating_add(expected.root_identity.as_bytes().len())
            .saturating_add(expected.root_binding_identity.len())
            .saturating_add(expected.database_identity.len());
        let mut bytes = size_of::<SourceManifestSnapshotPage>()
            .saturating_add(size_of::<SourceManifestEntry>().saturating_mul(requested_limit + 1))
            .saturating_add(revision_bytes)
            .saturating_add(after.map_or(0, |cursor| cursor.raw_path.len()));
        if bytes > MAX_REQUEST_BYTES {
            return Err(SourceWriterError::SnapshotBudgetExceeded("request bytes"));
        }
        let filter = crate::sample_sources::supported_audio_where_clause();
        let cursor_predicate = if after.is_some() {
            " AND path > ?1 COLLATE BINARY"
        } else {
            ""
        };
        let limit_parameter = if after.is_some() { "?2" } else { "?1" };
        let sql = format!(
            "SELECT path, file_identity, content_hash, file_size, modified_ns
             FROM wav_files
             WHERE {filter} AND missing = 0{cursor_predicate}
             ORDER BY path ASC LIMIT {limit_parameter}"
        );
        let mut entries = Vec::with_capacity(requested_limit);
        let mut last_visible_path = None;
        let mut has_more = false;
        {
            let mut statement = transaction
                .prepare(&sql)
                .map_err(|error| map_snapshot_sql(error, cancellation, started))?;
            let fetch_limit = i64::try_from(requested_limit + 1).unwrap_or(i64::MAX);
            let mut rows = match after {
                Some(cursor) => statement.query(rusqlite::params![cursor.raw_path, fetch_limit]),
                None => statement.query(rusqlite::params![fetch_limit]),
            }
            .map_err(|error| map_snapshot_sql(error, cancellation, started))?;
            let mut decoded_rows = 0usize;
            while let Some(row) = rows
                .next()
                .map_err(|error| map_snapshot_sql(error, cancellation, started))?
            {
                decoded_rows += 1;
                let path_bytes = required_text_bytes(row, 0)?;
                let identity_bytes = optional_text_bytes(row, 1)?;
                let hash_bytes = optional_text_bytes(row, 2)?;
                bytes = bytes
                    .saturating_add(path_bytes.saturating_mul(2))
                    .saturating_add(identity_bytes)
                    .saturating_add(hash_bytes);
                if bytes > MAX_REQUEST_BYTES {
                    return Err(SourceWriterError::SnapshotBudgetExceeded("decoded bytes"));
                }
                let raw_path = row.get::<_, String>(0).map_err(|_| {
                    SourceWriterError::SnapshotMalformed("manifest path is malformed")
                })?;
                if decoded_rows > requested_limit {
                    has_more = true;
                    break;
                }
                let relative_path = parse_relative_path_from_db(&raw_path).map_err(|_| {
                    SourceWriterError::SnapshotMalformed("manifest path is invalid")
                })?;
                let file_size = row
                    .get::<_, i64>(3)
                    .map_err(|_| SourceWriterError::SnapshotMalformed("file size is malformed"))?;
                let file_size = u64::try_from(file_size)
                    .map_err(|_| SourceWriterError::SnapshotMalformed("file size is negative"))?;
                entries.push(SourceManifestEntry {
                    relative_path,
                    file_identity: row.get(1).map_err(|_| {
                        SourceWriterError::SnapshotMalformed("file identity is malformed")
                    })?,
                    content_hash: row.get(2).map_err(|_| {
                        SourceWriterError::SnapshotMalformed("content hash is malformed")
                    })?,
                    file_size,
                    modified_ns: row.get(4).map_err(|_| {
                        SourceWriterError::SnapshotMalformed("modified time is malformed")
                    })?,
                });
                last_visible_path = Some(raw_path);
                check_budget(started, cancellation)?;
            }
        }
        transaction
            .rollback()
            .map_err(|error| map_snapshot_sql(error, cancellation, started))?;
        check_budget(started, cancellation)?;
        publication_fence
            .validate()
            .map_err(|_| SourceWriterError::StaleFence)?;
        let final_revision = self.current_revision()?;
        ensure_snapshot_revision(expected, &final_revision)?;
        check_budget(started, cancellation)?;
        if has_more {
            bytes = bytes
                .saturating_add(size_of::<SourceManifestSnapshotCursor>())
                .saturating_add(revision_bytes)
                .saturating_add(last_visible_path.as_ref().map_or(0, String::len));
            if bytes > MAX_REQUEST_BYTES {
                return Err(SourceWriterError::SnapshotBudgetExceeded(
                    "continuation bytes",
                ));
            }
        }
        Ok(SourceManifestSnapshotPage {
            revision: expected.clone(),
            entries,
            next_cursor: if has_more {
                last_visible_path.map(|raw_path| SourceManifestSnapshotCursor {
                    revision: expected.clone(),
                    raw_path,
                })
            } else {
                None
            },
            publication_fence,
        })
    }
}

fn ensure_snapshot_revision(
    expected: &SourceRevision,
    current: &SourceRevision,
) -> Result<(), SourceWriterError> {
    if expected.source_id != current.source_id
        || expected.root_identity != current.root_identity
        || expected.root_binding_identity != current.root_binding_identity
        || expected.database_identity != current.database_identity
        || expected.lifecycle_generation != current.lifecycle_generation
        || expected.lease_epoch != current.lease_epoch
    {
        return Err(SourceWriterError::StaleFence);
    }
    if expected.revision != current.revision {
        return Err(SourceWriterError::StaleRevision {
            expected: expected.revision,
            actual: current.revision,
        });
    }
    Ok(())
}

fn required_text_bytes(row: &Row<'_>, index: usize) -> Result<usize, SourceWriterError> {
    match row
        .get_ref(index)
        .map_err(|_| SourceWriterError::SnapshotMalformed("manifest value is malformed"))?
    {
        ValueRef::Text(bytes) => Ok(bytes.len()),
        _ => Err(SourceWriterError::SnapshotMalformed(
            "manifest text value is missing or malformed",
        )),
    }
}

fn optional_text_bytes(row: &Row<'_>, index: usize) -> Result<usize, SourceWriterError> {
    match row
        .get_ref(index)
        .map_err(|_| SourceWriterError::SnapshotMalformed("manifest value is malformed"))?
    {
        ValueRef::Null => Ok(0),
        ValueRef::Text(bytes) => Ok(bytes.len()),
        _ => Err(SourceWriterError::SnapshotMalformed(
            "optional manifest text is malformed",
        )),
    }
}

fn check_budget(started: Instant, cancellation: &Arc<AtomicBool>) -> Result<(), SourceWriterError> {
    if cancellation.load(Ordering::Acquire) {
        return Err(SourceDbError::Canceled.into());
    }
    if started.elapsed() >= MAX_REQUEST_TIME {
        return Err(SourceWriterError::SnapshotBudgetExceeded("elapsed time"));
    }
    Ok(())
}

fn map_snapshot_sql(
    error: rusqlite::Error,
    cancellation: &Arc<AtomicBool>,
    started: Instant,
) -> SourceWriterError {
    if matches!(
        &error,
        rusqlite::Error::SqliteFailure(sql_error, _)
            if sql_error.code == ErrorCode::OperationInterrupted
    ) {
        if cancellation.load(Ordering::Acquire) {
            return SourceDbError::Canceled.into();
        }
        if started.elapsed() >= MAX_REQUEST_TIME {
            return SourceWriterError::SnapshotBudgetExceeded("elapsed time");
        }
    }
    map_sql_error(error).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_snapshot_progress_interrupts_and_restores_connection() {
        let connection = Connection::open_in_memory().expect("in-memory SQLite");
        let cancellation = Arc::new(AtomicBool::new(false));
        let started = Instant::now() - MAX_REQUEST_TIME;
        let progress = SnapshotProgressGuard::install(&connection, &cancellation, started);
        let error = connection
            .query_row(
                "WITH RECURSIVE integers(n) AS (
                   SELECT 1 UNION ALL SELECT n + 1 FROM integers WHERE n < 1000000
                 ) SELECT sum(n) FROM integers",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect_err("elapsed budget interrupts SQLite work");
        assert!(matches!(
            map_snapshot_sql(error, &cancellation, started),
            SourceWriterError::SnapshotBudgetExceeded("elapsed time")
        ));
        drop(progress);
        assert_eq!(
            connection
                .query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                .expect("progress handler was removed"),
            1
        );
    }

    #[test]
    fn canceled_snapshot_progress_interrupts_sql_work() {
        let connection = Connection::open_in_memory().expect("in-memory SQLite");
        let cancellation = Arc::new(AtomicBool::new(true));
        let started = Instant::now();
        let progress = SnapshotProgressGuard::install(&connection, &cancellation, started);
        let error = connection
            .query_row(
                "WITH RECURSIVE integers(n) AS (
                   SELECT 1 UNION ALL SELECT n + 1 FROM integers WHERE n < 1000000
                 ) SELECT sum(n) FROM integers",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect_err("cancelled work interrupts SQLite");
        assert!(matches!(
            map_snapshot_sql(error, &cancellation, started),
            SourceWriterError::Database(SourceDbError::Canceled)
        ));
        drop(progress);
    }
}
