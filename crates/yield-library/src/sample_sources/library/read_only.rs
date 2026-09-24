use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rusqlite::{Connection, ErrorCode, OpenFlags};

use super::connection::validate_database_entries;
use super::error::map_sql_error;
use super::sources::load_sources_from_connection;
use super::{LibraryError, SourceRegistrySnapshot};

const READ_ONLY_OBSERVATION_BUDGET: Duration = Duration::from_secs(2);
const READ_ONLY_PROGRESS_OPS: i32 = 1_000;

/// Read the active global source registry without creating, migrating, or writing the database.
///
/// The returned snapshot is an observation of the database at one bounded read transaction. It
/// does not retain a connection, profile ownership, source authority, or a freshness guarantee
/// after the transaction closes. Strict filesystem no-write and WAL/SHM qualification remain a
/// separate platform policy boundary.
pub fn load_source_registry_snapshot_read_only(
    database_path: &Path,
) -> Result<SourceRegistrySnapshot, LibraryError> {
    observe_source_registry(
        database_path,
        Instant::now() + READ_ONLY_OBSERVATION_BUDGET,
        None,
    )
}

#[cfg(test)]
pub(super) fn load_source_registry_snapshot_read_only_with_deadline_for_test(
    database_path: &Path,
    deadline: Instant,
    interrupt: Arc<AtomicBool>,
) -> Result<SourceRegistrySnapshot, LibraryError> {
    observe_source_registry(database_path, deadline, Some(interrupt))
}

fn observe_source_registry(
    database_path: &Path,
    deadline: Instant,
    interrupt: Option<Arc<AtomicBool>>,
) -> Result<SourceRegistrySnapshot, LibraryError> {
    let parent = database_path
        .parent()
        .ok_or(LibraryError::DatabasePathValidation {
            path: database_path.to_path_buf(),
            reason: String::from("database path has no parent directory"),
        })?;
    if database_path.file_name() != Some(OsStr::new(super::LIBRARY_DB_FILE_NAME)) {
        return Err(LibraryError::DatabasePathValidation {
            path: database_path.to_path_buf(),
            reason: String::from("read-only observation requires library.db"),
        });
    }
    check_budget(deadline, interrupt.as_ref())?;
    validate_database_entries(parent)?;
    let metadata = std::fs::symlink_metadata(database_path).map_err(|source| {
        LibraryError::DatabasePathValidation {
            path: database_path.to_path_buf(),
            reason: source.to_string(),
        }
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(LibraryError::DatabasePathValidation {
            path: database_path.to_path_buf(),
            reason: String::from("database entry is not a regular file"),
        });
    }

    let mut connection = Connection::open_with_flags(
        database_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(map_observation_sql_error)?;
    connection
        .busy_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(map_observation_sql_error)?;
    let progress_interrupt = interrupt.clone();
    connection.progress_handler(
        READ_ONLY_PROGRESS_OPS,
        Some(move || {
            Instant::now() >= deadline
                || progress_interrupt
                    .as_ref()
                    .is_some_and(|flag| flag.load(Ordering::Acquire))
        }),
    );

    let result = (|| {
        check_budget(deadline, interrupt.as_ref())?;
        ensure_read_only_schema(&connection)?;
        check_budget(deadline, interrupt.as_ref())?;
        let transaction = connection
            .transaction()
            .map_err(map_observation_sql_error)?;
        let sources =
            load_sources_from_connection(&transaction).map_err(normalize_observation_error)?;
        check_budget(deadline, interrupt.as_ref())?;
        transaction.commit().map_err(map_observation_sql_error)?;
        check_budget(deadline, interrupt.as_ref())?;
        Ok(SourceRegistrySnapshot::from_sources(sources))
    })();
    connection.progress_handler(0, Option::<fn() -> bool>::None);
    result
}

fn ensure_read_only_schema(connection: &Connection) -> Result<(), LibraryError> {
    ensure_table_columns(
        connection,
        "metadata",
        &["key", "value"],
        "metadata table or column missing",
    )?;
    ensure_table_columns(
        connection,
        "sources",
        &[
            "id",
            "root",
            "sort_order",
            "role",
            "metadata_storage",
            "primary_import_folder",
        ],
        "sources table or column missing",
    )?;
    Ok(())
}

fn ensure_table_columns(
    connection: &Connection,
    table: &str,
    required: &[&str],
    reason: &'static str,
) -> Result<(), LibraryError> {
    let mut statement = connection
        .prepare(&format!(
            "SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1 LIMIT 1"
        ))
        .map_err(map_observation_sql_error)?;
    for column in required {
        if !statement
            .exists([column])
            .map_err(map_observation_sql_error)?
        {
            return Err(LibraryError::ReadOnlySchemaIncompatible { reason });
        }
    }
    Ok(())
}

fn check_budget(
    deadline: Instant,
    interrupt: Option<&Arc<AtomicBool>>,
) -> Result<(), LibraryError> {
    if Instant::now() >= deadline || interrupt.is_some_and(|flag| flag.load(Ordering::Acquire)) {
        return Err(LibraryError::ReadOnlyObservationBudgetExceeded);
    }
    Ok(())
}

fn normalize_observation_error(error: LibraryError) -> LibraryError {
    match error {
        LibraryError::Sql(error)
            if matches!(
                &error,
                rusqlite::Error::SqliteFailure(error, _)
                    if error.code == ErrorCode::OperationInterrupted
            ) =>
        {
            LibraryError::ReadOnlyObservationBudgetExceeded
        }
        error => error,
    }
}

fn map_observation_sql_error(error: rusqlite::Error) -> LibraryError {
    if matches!(
        &error,
        rusqlite::Error::SqliteFailure(error, _)
            if error.code == ErrorCode::OperationInterrupted
    ) {
        return LibraryError::ReadOnlyObservationBudgetExceeded;
    }
    map_sql_error(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard, WritableProfileGuard};
    use crate::sample_sources::{
        GlobalLibraryWriter, GlobalLibraryWriterStatus, LibraryState, SampleSource,
    };
    use crate::test_runtime::TestRuntimeGuard;
    use std::sync::atomic::AtomicBool;
    use std::thread;
    use std::time::{Duration, Instant};
    use tempfile::tempdir;

    fn prepare_profile(
        name: &str,
    ) -> (
        tempfile::TempDir,
        ConfigBaseGuard,
        PersistenceProfileGuard,
        WritableProfileGuard,
    ) {
        let temp = tempdir().expect("base");
        let base =
            ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).expect("canonical base"));
        let profile = PersistenceProfileGuard::named(name);
        let writable = WritableProfileGuard::acquire_current().expect("profile ownership");
        (temp, base, profile, writable)
    }

    #[test]
    fn read_only_snapshot_uses_existing_schema_while_owner_is_open() {
        let _runtime = TestRuntimeGuard::acquire();
        let (_temp, _base, _profile, writable) = prepare_profile("readonly-observation");
        let root = tempfile::tempdir().expect("source root");
        crate::sample_sources::library::save(&LibraryState {
            sources: vec![SampleSource::new_with_id(
                crate::sample_sources::SourceId::from_string("readonly-source"),
                root.path().to_path_buf(),
            )],
        })
        .expect("seed library");
        let database_path = writable
            .profile_root()
            .join(super::super::LIBRARY_DB_FILE_NAME);
        let mut writer = GlobalLibraryWriter::start(&writable).expect("writer start");
        let deadline = Instant::now() + Duration::from_secs(2);
        while matches!(writer.status(), GlobalLibraryWriterStatus::Initializing) {
            assert!(Instant::now() < deadline, "owner did not initialize");
            thread::sleep(Duration::from_millis(5));
        }
        let loaded = writer
            .load_source_registry_snapshot()
            .expect("owner load admission")
            .recv_timeout(Duration::from_secs(2))
            .expect("owner load reply")
            .expect("owner load");
        assert_eq!(loaded.len(), 1);
        assert!(matches!(
            writer.status(),
            GlobalLibraryWriterStatus::Available
        ));

        let observed =
            load_source_registry_snapshot_read_only(&database_path).expect("read-only observation");
        assert_eq!(observed.len(), 1);
        assert_eq!(observed.as_slice()[0].id.as_str(), "readonly-source");
        assert!(writer.shutdown());
    }

    #[test]
    fn read_only_snapshot_does_not_create_missing_database() {
        let temp = tempdir().expect("base");
        let database_path = temp.path().join(super::super::LIBRARY_DB_FILE_NAME);
        let error = load_source_registry_snapshot_read_only(&database_path)
            .expect_err("missing database must fail closed");
        assert!(matches!(error, LibraryError::DatabasePathValidation { .. }));
        assert!(!database_path.exists());
    }

    #[test]
    fn read_only_snapshot_rejects_incompatible_schema_without_migration() {
        let _runtime = TestRuntimeGuard::acquire();
        let (_temp, _base, _profile, writable) = prepare_profile("readonly-incompatible");
        crate::sample_sources::library::save(&LibraryState::default()).expect("seed library");
        let database_path = writable
            .profile_root()
            .join(super::super::LIBRARY_DB_FILE_NAME);
        let connection = Connection::open(&database_path).expect("open initialized fixture");
        connection
            .execute("DROP TABLE sources", [])
            .expect("remove required table");
        drop(connection);

        let error = load_source_registry_snapshot_read_only(&database_path)
            .expect_err("missing sources schema");
        assert!(matches!(
            error,
            LibraryError::ReadOnlySchemaIncompatible { .. }
        ));
        let connection = Connection::open(&database_path).expect("reopen fixture");
        let metadata_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='metadata'",
                [],
                |row| row.get(0),
            )
            .expect("schema remains unchanged");
        assert_eq!(metadata_count, 1);
        let sources_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='sources'",
                [],
                |row| row.get(0),
            )
            .expect("removed table remains absent");
        assert_eq!(sources_count, 0);
    }

    #[test]
    fn read_only_observation_deadline_interrupts_before_opening_work() {
        let temp = tempdir().expect("base");
        let database_path = temp.path().join(super::super::LIBRARY_DB_FILE_NAME);
        let connection = Connection::open(&database_path).expect("create fixture");
        connection
            .execute(
                "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
                [],
            )
            .expect("metadata fixture");
        drop(connection);
        let interrupt = Arc::new(AtomicBool::new(true));
        let error = load_source_registry_snapshot_read_only_with_deadline_for_test(
            &database_path,
            Instant::now() + Duration::from_secs(2),
            interrupt,
        )
        .expect_err("interrupted observation");
        assert!(matches!(
            error,
            LibraryError::ReadOnlyObservationBudgetExceeded
        ));
    }

    #[test]
    fn read_only_schema_accepts_required_columns_after_extra_columns() {
        let connection = Connection::open_in_memory().expect("fixture");
        connection
            .execute_batch(
                "CREATE TABLE metadata (extra TEXT, key TEXT, value TEXT);
                 CREATE TABLE sources (
                     extra_a TEXT, extra_b TEXT, id TEXT, root TEXT,
                     sort_order INTEGER, role TEXT, metadata_storage TEXT,
                     primary_import_folder TEXT
                 );",
            )
            .expect("compatible fixture");
        ensure_read_only_schema(&connection).expect("required columns may follow extra columns");
    }
}
