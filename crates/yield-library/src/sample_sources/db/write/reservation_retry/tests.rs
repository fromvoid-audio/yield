use super::*;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<BTreeMap<String, String>>>>);
impl Subscriber for Capture {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.target() == "yield::source_writer_acquisition"
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        struct Fields(BTreeMap<String, String>);
        impl Visit for Fields {
            fn record_str(&mut self, field: &Field, value: &str) {
                self.0.insert(field.name().into(), value.into());
            }
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                self.0.insert(field.name().into(), format!("{value:?}"));
            }
        }
        let mut fields = Fields(BTreeMap::new());
        event.record(&mut fields);
        self.0.lock().unwrap().push(fields.0);
    }
}
fn capture<T>(run: impl FnOnce() -> T) -> (T, Vec<BTreeMap<String, String>>) {
    let capture = Capture::default();
    let result = tracing::subscriber::with_default(capture.clone(), run);
    let records = capture.0.lock().unwrap().clone();
    (result, records)
}
fn observed(
    records: &[BTreeMap<String, String>],
    outcome: &str,
    attempts: u8,
    retries: u8,
    code: i32,
) {
    assert_eq!(
        records,
        &[BTreeMap::from([
            ("attempts".into(), attempts.to_string()),
            ("retries".into(), retries.to_string()),
            ("sqlite_code".into(), code.to_string()),
            ("outcome".into(), outcome.into()),
        ])]
    );
}
fn timeout(database: &SourceDatabase) -> u64 {
    database
        .connection
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .unwrap()
}
fn plain() -> (tempfile::TempDir, SourceDatabase) {
    let root = tempfile::tempdir().unwrap();
    let database = SourceDatabase::open_for_source_write(root.path()).unwrap();
    database
        .connection
        .busy_timeout(Duration::from_millis(1234))
        .unwrap();
    (root, database)
}

#[test]
fn reservation_retry_busy_exhaustion_has_exact_attempts_no_body_or_revision() {
    let (_root, database) = plain();
    database.upsert_file(Path::new("one.wav"), 10, 1).unwrap();
    let revision = database.get_revision().unwrap();
    let rival = rusqlite::Connection::open(&database.db_path).unwrap();
    rival.execute_batch("BEGIN IMMEDIATE").unwrap();
    let mut backoffs = Vec::new();
    let (result, records) =
        capture(|| begin_with_backoff(&database, |attempt| backoffs.push(attempt)));
    assert!(matches!(result, Err(SourceDbError::Busy)));
    observed(&records, "busy_exhausted", 3, 2, ffi::SQLITE_BUSY);
    assert_eq!(backoffs, [1, 2]);
    assert_eq!(timeout(&database), 1234);
    assert!(database.connection.is_autocommit());
    assert_eq!(database.get_revision().unwrap(), revision);
    assert_eq!(database.list_files().unwrap().len(), 1);
    let (_, records) =
        capture(|| assert!(matches!(database.write_batch(), Err(SourceDbError::Busy))));
    observed(&records, "busy_exhausted", 3, 2, ffi::SQLITE_BUSY);
    rival.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn reservation_retry_released_lock_returns_one_transaction_and_restores_timeout() {
    let (_root, database) = plain();
    database
        .connection
        .execute_batch("CREATE TABLE effect(value UNIQUE)")
        .unwrap();
    let rival = rusqlite::Connection::open(&database.db_path).unwrap();
    rival.execute_batch("BEGIN IMMEDIATE").unwrap();
    let mut backoffs = Vec::new();
    let (result, records) = capture(|| {
        begin_with_backoff(&database, |attempt| {
            assert_eq!(timeout(&database), 0);
            backoffs.push(attempt);
            if attempt == 2 {
                rival.execute_batch("ROLLBACK").unwrap();
            }
        })
    });
    let transaction = result.unwrap();
    observed(&records, "acquired", 3, 2, ffi::SQLITE_BUSY);
    assert_eq!(backoffs, [1, 2]);
    assert_eq!(timeout(&database), 1234);
    transaction
        .execute("INSERT INTO effect VALUES (1)", [])
        .unwrap();
    transaction.commit().unwrap();
    let count: u64 = database
        .connection
        .query_row("SELECT count(*) FROM effect", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn reservation_retry_locked_extended_errors_and_body_errors_are_not_replayed() {
    let (_root, mut database) = plain();
    let flags =
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_SHARED_CACHE;
    database.connection = rusqlite::Connection::open_with_flags(&database.db_path, flags)
        .unwrap()
        .into();
    database
        .connection
        .busy_timeout(Duration::from_millis(800))
        .unwrap();
    let rival = rusqlite::Connection::open_with_flags(&database.db_path, flags).unwrap();
    rival.execute_batch("BEGIN IMMEDIATE").unwrap();
    let (result, records) =
        capture(|| begin_with_backoff(&database, |_| panic!("LOCKED must not retry")));
    assert!(
        matches!(result, Err(SourceDbError::Sql(rusqlite::Error::SqliteFailure(error, _)))
        if error.extended_code == ffi::SQLITE_LOCKED_SHAREDCACHE)
    );
    observed(
        &records,
        "contention_not_retryable",
        1,
        0,
        ffi::SQLITE_LOCKED_SHAREDCACHE,
    );
    assert_eq!(timeout(&database), 800);
    rival.execute_batch("ROLLBACK").unwrap();
    for code in [
        ffi::SQLITE_BUSY_SNAPSHOT,
        ffi::SQLITE_BUSY_RECOVERY,
        ffi::SQLITE_LOCKED,
    ] {
        let error =
            rusqlite::Error::SqliteFailure(ffi::Error::new(code), Some("private-error".into()));
        assert!(!retryable(&error));
        assert!(contention(&error));
        assert!(
            matches!(map_sql_error(error), SourceDbError::Sql(rusqlite::Error::SqliteFailure(error, _))
            if error.extended_code == code)
        );
    }
    database
        .connection
        .execute_batch("PRAGMA query_only=ON")
        .unwrap();
    let (result, records) =
        capture(|| begin_with_backoff(&database, |_| panic!("noncontention must not retry")));
    assert!(matches!(result, Err(SourceDbError::Sql(_))));
    observed(&records, "sqlite_error", 1, 0, ffi::SQLITE_READONLY);
    assert_eq!(timeout(&database), 800);
    database.connection.execute_batch("PRAGMA query_only=OFF; CREATE TABLE effect(value UNIQUE); INSERT INTO effect VALUES (1)").unwrap();
    let (transaction, records) = capture(|| begin(&database).unwrap());
    observed(&records, "acquired", 1, 0, 0);
    assert!(
        transaction
            .execute("INSERT INTO effect VALUES (1)", [])
            .is_err()
    );
    transaction.rollback().unwrap();
    let count: u64 = database
        .connection
        .query_row("SELECT count(*) FROM effect", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn reservation_retry_rejects_active_transaction_without_disturbing_it() {
    let (_root, database) = plain();
    let transaction = begin(&database).unwrap();
    let (result, records) =
        capture(|| begin_with_backoff(&database, |_| panic!("nested must not retry")));
    assert!(matches!(result, Err(SourceDbError::Unexpected)));
    observed(&records, "rejected", 0, 0, 0);
    assert!(!database.connection.is_autocommit());
    assert_eq!(timeout(&database), 1234);
    transaction.rollback().unwrap();
}

struct Owned {
    database: SourceDatabase,
    profile: crate::app_dirs::WritableProfileGuard,
    _source: tempfile::TempDir,
    _profile: crate::app_dirs::PersistenceProfileGuard,
    _base_guard: crate::app_dirs::ConfigBaseGuard,
    _base: tempfile::TempDir,
    _runtime: crate::test_runtime::TestRuntimeGuard,
}
fn owned() -> Owned {
    let runtime = crate::test_runtime::TestRuntimeGuard::acquire();
    let temp = std::fs::canonicalize(std::env::temp_dir()).unwrap();
    let base = tempfile::tempdir_in(temp).unwrap();
    let base_guard = crate::app_dirs::ConfigBaseGuard::set(base.path().into());
    let profile_guard = crate::app_dirs::PersistenceProfileGuard::named("source-reservation-retry");
    let profile = crate::app_dirs::WritableProfileGuard::acquire_current().unwrap();
    let source = tempfile::tempdir().unwrap();
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        source.path(),
        &profile,
        Path::new("source-retry"),
    )
    .unwrap();
    database
        .connection
        .busy_timeout(Duration::from_millis(777))
        .unwrap();
    Owned {
        database,
        profile,
        _source: source,
        _profile: profile_guard,
        _base_guard: base_guard,
        _base: base,
        _runtime: runtime,
    }
}

#[test]
fn reservation_retry_durable_lease_expiry_during_wait_is_not_renewed() {
    let fixture = owned();
    let database = &fixture.database;
    let rival = rusqlite::Connection::open(&database.db_path).unwrap();
    rival.execute_batch("BEGIN IMMEDIATE").unwrap();
    let (result, records) = capture(|| {
        begin_with_backoff(database, |attempt| {
            assert_eq!(attempt, 1);
            rival
                .execute_batch("UPDATE source_lease SET expires_at = acquired_at; COMMIT")
                .unwrap();
        })
    });
    assert!(matches!(
        result,
        Err(SourceDbError::SourceLease(
            crate::sample_sources::db::SourceLeaseError::Expired { .. }
        ))
    ));
    observed(&records, "ownership_rejected", 1, 1, ffi::SQLITE_BUSY);
    assert_eq!(timeout(database), 777);
    assert!(database.connection.is_autocommit());
    let expired: bool = database
        .connection
        .query_row(
            "SELECT expires_at = acquired_at FROM source_lease",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(expired);
}

#[test]
fn reservation_retry_near_expiry_success_does_not_renew_during_wait() {
    let fixture = owned();
    let database = &fixture.database;
    let rival = rusqlite::Connection::open(&database.db_path).unwrap();
    rival.execute_batch("BEGIN IMMEDIATE").unwrap();
    let (result, records) = capture(|| {
        begin_with_backoff(database, |attempt| {
            assert_eq!(attempt, 1);
            rival
                .execute_batch("UPDATE source_lease SET expires_at = acquired_at + 30; COMMIT")
                .unwrap();
        })
    });
    let transaction = result.unwrap();
    observed(&records, "acquired", 2, 1, ffi::SQLITE_BUSY);
    let remaining: i64 = transaction
        .query_row(
            "SELECT expires_at - acquired_at FROM source_lease",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(remaining, 30);
    assert_eq!(timeout(database), 777);
    transaction.rollback().unwrap();
}

#[cfg(unix)]
#[test]
fn reservation_retry_namespace_replacement_during_wait_retains_data() {
    let fixture = owned();
    let database = &fixture.database;
    database.upsert_file(Path::new("one.wav"), 10, 1).unwrap();
    let revision = database.get_revision().unwrap();
    let rival = rusqlite::Connection::open(&database.db_path).unwrap();
    rival.execute_batch("BEGIN IMMEDIATE").unwrap();
    let child = fixture.profile.profile_root().join("source-retry");
    let displaced = fixture
        .profile
        .profile_root()
        .join("displaced-source-retry");
    let (result, records) = capture(|| {
        begin_with_backoff(database, |_| {
            rival.execute_batch("ROLLBACK").unwrap();
            std::fs::rename(&child, &displaced).unwrap();
            std::fs::create_dir(&child).unwrap();
            std::fs::write(child.join("private-sentinel"), b"untouched").unwrap();
        })
    });
    assert!(result.is_err());
    observed(&records, "ownership_rejected", 1, 1, ffi::SQLITE_BUSY);
    assert_eq!(timeout(database), 777);
    assert!(database.connection.is_autocommit());
    assert_eq!(database.get_revision().unwrap(), revision);
    assert_eq!(
        std::fs::read(child.join("private-sentinel")).unwrap(),
        b"untouched"
    );
    std::fs::remove_dir_all(&child).unwrap();
    std::fs::rename(&displaced, &child).unwrap();
}

#[test]
fn reservation_retry_actual_source_writer_preserves_receipts_and_restart_replay() {
    use crate::sample_sources::{
        Rating, SourceCommandId, SourceCurationUpdate, SourceId, SourceWriter, SourceWriterError,
        SourceWriterRequest,
    };
    let fixture = owned();
    fixture
        .database
        .upsert_file(Path::new("one.wav"), 10, 1)
        .unwrap();
    fixture
        .database
        .begin_source_directory_truth_generation(1, 0)
        .unwrap();
    let path = fixture.database.db_path.clone();
    let source = fixture.database.root.clone();
    let mut writer =
        SourceWriter::new_for_scan(fixture.database, SourceId::from_string("retry-source"), 1)
            .unwrap();
    let fence = writer.current_scan_fence().unwrap();
    let audit = writer.catalog_database().write_batch().unwrap();
    audit
        .commit_manifest_audit_with_staged_directory_truth(
            1,
            1,
            1,
            fence.root_identity().clone(),
            None,
        )
        .unwrap();
    let expected = writer.current_revision().unwrap();
    let request = SourceWriterRequest::new(
        crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(511)),
        crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(512)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(513)),
        expected.clone(),
        vec![SourceCurationUpdate::SetRating {
            path: crate::sample_sources::reconciliation::RootRelativePath::try_from_path(
                "one.wav".into(),
            )
            .unwrap(),
            rating: Rating::KEEP_1,
        }],
    )
    .unwrap();
    let rival = rusqlite::Connection::open(&path).unwrap();
    rival.execute_batch("BEGIN IMMEDIATE").unwrap();
    let (result, records) = capture(|| writer.commit_curation(request.clone()));
    assert!(matches!(
        result,
        Err(SourceWriterError::Database(SourceDbError::Busy))
    ));
    observed(&records, "busy_exhausted", 3, 2, ffi::SQLITE_BUSY);
    assert_eq!(
        writer.current_revision().unwrap().revision(),
        expected.revision()
    );
    let count: u64 = rival
        .query_row(
            "SELECT count(*) FROM source_participant_curation_receipts",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
    rival.execute_batch("ROLLBACK").unwrap();
    let receipt = writer.commit_curation(request.clone()).unwrap();
    assert_eq!(
        receipt.source_revision().revision(),
        expected.revision() + 1
    );
    assert_eq!(writer.commit_curation(request.clone()).unwrap(), receipt);
    drop(writer);
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        &source,
        &fixture.profile,
        Path::new("source-retry"),
    )
    .unwrap();
    let mut reopened =
        SourceWriter::new(database, SourceId::from_string("retry-source"), 2).unwrap();
    assert_eq!(
        reopened.current_revision().unwrap().revision(),
        receipt.source_revision().revision()
    );
    // Old acquisition request remains observation/replay evidence, never a fresh mutation.
    let before = reopened.current_revision().unwrap();
    assert!(matches!(
        reopened.commit_curation(request),
        Err(SourceWriterError::SupersededLifecycle { .. })
    ));
    assert_eq!(
        reopened.current_revision().unwrap().revision(),
        before.revision()
    );
    let count: u64 = rival
        .query_row(
            "SELECT count(*) FROM source_participant_curation_receipts",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}
