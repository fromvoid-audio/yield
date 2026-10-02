use super::*;
use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard, WritableProfileGuard};
use crate::sample_sources::library::*;
use crate::test_runtime::TestRuntimeGuard;
use std::time::Instant;

fn assert_busy<T: std::fmt::Debug>(result: Result<T, LibraryError>) {
    assert!(
        matches!(
            result,
            Err(LibraryError::OrdinaryWriteContention {
                attempts: MAX_ATTEMPTS,
                sqlite_code: ffi::SQLITE_BUSY
            })
        ),
        "{result:?}"
    );
}

#[test]
fn ordinary_retry_released_reservation_executes_body_once_and_restores_timeout() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("test.sqlite");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE effects(value);")
        .unwrap();
    connection
        .busy_timeout(Duration::from_millis(1234))
        .unwrap();
    let rival = Connection::open(&path).unwrap();
    rival.execute_batch("BEGIN IMMEDIATE").unwrap();
    let metrics = metrics::AcquisitionMetrics::new();
    let mut retries = vec![];
    let transaction = begin_observed(
        &connection,
        |attempt| {
            retries.push(attempt);
            std::thread::sleep(Duration::from_millis(10));
            if attempt == 2 {
                rival.execute_batch("ROLLBACK").unwrap();
            }
        },
        &metrics,
    )
    .unwrap();
    assert_eq!(retries, [1, 2]);
    let snapshot = metrics.snapshot();
    assert_eq!(
        (snapshot.attempts, snapshot.retries, snapshot.acquired),
        (3, 2, 1)
    );
    assert_eq!(snapshot.elapsed_buckets.iter().sum::<u64>(), 1);
    // The measured boundary includes both real pre-effect backoffs.
    assert_eq!(&snapshot.elapsed_buckets[..2], &[0, 0]);
    assert_eq!(
        connection
            .query_row("PRAGMA busy_timeout", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        1234
    );
    transaction
        .execute("INSERT INTO effects VALUES (1)", [])
        .unwrap();
    transaction.commit().unwrap();
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM effects", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn ordinary_retry_exhaustion_and_noncontention_restore_timeout_without_effects() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("test.sqlite");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE effects(value);")
        .unwrap();
    connection.busy_timeout(Duration::from_millis(900)).unwrap();
    let rival = Connection::open(&path).unwrap();
    rival.execute_batch("BEGIN IMMEDIATE").unwrap();
    let metrics = metrics::AcquisitionMetrics::new();
    let mut retries = vec![];
    assert_busy(begin_observed(
        &connection,
        |attempt| retries.push(attempt),
        &metrics,
    ));
    let snapshot = metrics.snapshot();
    assert_eq!(
        (snapshot.attempts, snapshot.retries, snapshot.busy_exhausted),
        (3, 2, 1)
    );
    assert_eq!(snapshot.elapsed_buckets.iter().sum::<u64>(), 1);
    assert_eq!(retries, [1, 2]);
    assert!(connection.is_autocommit());
    assert_eq!(
        connection
            .query_row("PRAGMA busy_timeout", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        900
    );
    rival.execute_batch("ROLLBACK").unwrap();
    connection.execute_batch("PRAGMA query_only=ON").unwrap();
    let mut retries = 0;
    assert!(matches!(
        begin_observed(&connection, |_| retries += 1, &metrics),
        Err(LibraryError::Sql(_))
    ));
    assert_eq!(retries, 0);
    let snapshot = metrics.snapshot();
    assert_eq!(
        (snapshot.attempts, snapshot.retries, snapshot.other_failure),
        (4, 2, 1)
    );
    assert_eq!(snapshot.elapsed_buckets.iter().sum::<u64>(), 2);
    assert_eq!(
        connection
            .query_row("PRAGMA busy_timeout", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        900
    );
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM effects", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn ordinary_retry_does_not_retry_effect_errors_or_nested_transaction() {
    let connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch("CREATE TABLE effects(value UNIQUE); INSERT INTO effects VALUES (1);")
        .unwrap();
    let transaction = begin(&connection).unwrap();
    assert!(
        transaction
            .execute("INSERT INTO effects VALUES (1)", [])
            .is_err()
    );
    let metrics = metrics::AcquisitionMetrics::new();
    assert!(matches!(
        begin_observed(&connection, |_| panic!("nested retry"), &metrics),
        Err(LibraryError::Sql(_))
    ));
    let snapshot = metrics.snapshot();
    assert_eq!(
        (
            snapshot.attempts,
            snapshot.retries,
            snapshot.nested_transaction
        ),
        (0, 0, 1)
    );
    assert_eq!(snapshot.elapsed_buckets.iter().sum::<u64>(), 1);
    transaction.rollback().unwrap();
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM effects", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        1
    );
    for code in [
        ffi::SQLITE_LOCKED,
        ffi::SQLITE_BUSY_SNAPSHOT,
        ffi::SQLITE_BUSY_RECOVERY,
    ] {
        let error = rusqlite::Error::SqliteFailure(ffi::Error::new(code), None);
        assert_eq!(contention_code(&error), Some(code));
        assert_ne!(contention_code(&error), Some(ffi::SQLITE_BUSY));
    }
}

fn available(writer: &GlobalLibraryWriter) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while writer.status() != GlobalLibraryWriterStatus::Available {
        assert!(Instant::now() < deadline, "{:?}", writer.status());
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn complete(
    writer: &GlobalLibraryWriter,
    request: GlobalMutationRequest,
) -> Result<GlobalMutationValue, LibraryError> {
    let context = request.context().clone();
    writer
        .submit_captured_mutation(request)
        .unwrap()
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .into_result_for(&context)
}

#[test]
fn ordinary_retry_real_worker_all_routes_retain_state_and_replay_after_contention() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    let _profile = PersistenceProfileGuard::named("ordinary-busy");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let mut writer = GlobalLibraryWriter::start(&guard).unwrap();
    available(&writer);
    let acquisition = writer.acquisition_id();
    complete(
        &writer,
        GlobalMutationRequest::preferences(acquisition.clone(), 1, "committed".into()).unwrap(),
    )
    .unwrap();
    complete(
        &writer,
        GlobalMutationRequest::vocabulary_set(
            acquisition.clone(),
            1,
            0,
            "kick".into(),
            ProfileTagCategory::SoundType,
        )
        .unwrap(),
    )
    .unwrap();
    let rival = Connection::open(guard.profile_root().join(LIBRARY_DB_FILE_NAME)).unwrap();
    rival.execute_batch("BEGIN IMMEDIATE").unwrap();
    let prefs = GlobalMutationRequest::preferences(acquisition.clone(), 2, "next".into()).unwrap();
    let registry = GlobalMutationRequest::registry(
        acquisition.clone(),
        1,
        SourceRegistrySnapshot::new(vec![]),
    )
    .unwrap();
    let set = GlobalMutationRequest::vocabulary_set(
        acquisition.clone(),
        2,
        1,
        "kick".into(),
        ProfileTagCategory::Character,
    )
    .unwrap();
    let delete =
        GlobalMutationRequest::vocabulary_delete(acquisition.clone(), 3, 1, "kick".into()).unwrap();
    let before = global_acquisition_metrics_snapshot();
    for request in [&prefs, &registry, &set, &delete] {
        assert_busy(complete(&writer, request.clone()));
    }
    let after = global_acquisition_metrics_snapshot();
    // Other tests may acquire concurrently; process totals must include all four real routes.
    assert!(after.attempts >= before.attempts + 12);
    assert!(after.retries >= before.retries + 8);
    assert!(after.busy_exhausted >= before.busy_exhausted + 4);
    assert!(
        after.elapsed_buckets.iter().sum::<u64>() >= before.elapsed_buckets.iter().sum::<u64>() + 4
    );
    assert_eq!(
        writer
            .load_ui_preferences_payload()
            .unwrap()
            .recv()
            .unwrap()
            .unwrap()
            .as_deref(),
        Some("committed")
    );
    assert_eq!(
        writer
            .lookup_profile_tag_categories(vec!["kick".into()])
            .unwrap()
            .recv()
            .unwrap()
            .unwrap()
            .revision,
        1
    );
    rival.execute_batch("ROLLBACK").unwrap();
    for request in [&prefs, &registry] {
        complete(&writer, request.clone()).unwrap();
        complete(&writer, request.clone()).unwrap();
    }
    complete(&writer, set.clone()).unwrap();
    assert!(matches!(
        complete(&writer, set),
        Err(LibraryError::ProfileTagVocabularyStaleRevision {
            expected: 1,
            actual: 2
        })
    ));
    assert_eq!(
        writer
            .lookup_profile_tag_categories(vec!["kick".into()])
            .unwrap()
            .recv()
            .unwrap()
            .unwrap()
            .revision,
        2
    );
    assert!(writer.shutdown());
    drop(writer);
    drop(rival);
    let mut restarted = GlobalLibraryWriter::start(&guard).unwrap();
    available(&restarted);
    assert_eq!(
        restarted
            .load_ui_preferences_payload()
            .unwrap()
            .recv()
            .unwrap()
            .unwrap()
            .as_deref(),
        Some("next")
    );
    assert_eq!(
        restarted
            .lookup_profile_tag_categories(vec!["kick".into()])
            .unwrap()
            .recv()
            .unwrap()
            .unwrap()
            .revision,
        2
    );
    assert!(restarted.shutdown());
}

#[test]
fn ordinary_retry_real_shared_cache_locked_preserves_code_and_returns_without_retry() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("shared.sqlite");
    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
        | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
        | rusqlite::OpenFlags::SQLITE_OPEN_SHARED_CACHE;
    let connection = Connection::open_with_flags(&path, flags).unwrap();
    connection
        .execute_batch("CREATE TABLE effects(value)")
        .unwrap();
    connection.busy_timeout(Duration::from_millis(800)).unwrap();
    let rival = Connection::open_with_flags(&path, flags).unwrap();
    rival.execute_batch("BEGIN IMMEDIATE").unwrap();
    let metrics = metrics::AcquisitionMetrics::new();
    let mut retries = 0;
    assert!(matches!(
        begin_observed(&connection, |_| retries += 1, &metrics),
        Err(LibraryError::OrdinaryWriteContention {
            attempts: 1,
            sqlite_code: ffi::SQLITE_LOCKED_SHAREDCACHE
        })
    ));
    assert_eq!(retries, 0);
    let snapshot = metrics.snapshot();
    assert_eq!(
        (
            snapshot.attempts,
            snapshot.retries,
            snapshot.contention_not_retried
        ),
        (1, 0, 1)
    );
    assert_eq!(snapshot.elapsed_buckets.iter().sum::<u64>(), 1);
    assert!(connection.is_autocommit());
    assert_eq!(
        connection
            .query_row("PRAGMA busy_timeout", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        800
    );
    rival.execute_batch("ROLLBACK").unwrap();
    begin(&connection).unwrap().rollback().unwrap();
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM effects", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        0
    );
}
