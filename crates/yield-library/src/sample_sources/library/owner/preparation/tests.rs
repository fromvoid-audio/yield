//! Real retained-worker/SQLite regressions; no journal or physical-provider fixture grants.
use super::*;
use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard};
use crate::test_runtime::TestRuntimeGuard;
use rusqlite::{Connection, OpenFlags};

fn fixture(test: impl FnOnce(&mut GlobalLibraryWriter, &WritableProfileGuard, &tempfile::TempDir)) {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    let _profile = PersistenceProfileGuard::named("global-preparation");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let mut writer = GlobalLibraryWriter::start(&guard).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while writer.status() != GlobalLibraryWriterStatus::Available {
        assert!(Instant::now() < deadline, "{:?}", writer.status());
        std::thread::sleep(Duration::from_millis(5));
    }
    test(&mut writer, &guard, &temp);
}
fn request(writer: &GlobalLibraryWriter, duration: Duration) -> GlobalPreparationRequest {
    GlobalPreparationRequest::new(
        writer.preparation_owner(),
        [7; 32],
        Instant::now() + duration,
        1,
        128,
    )
    .unwrap()
}
fn reserve(writer: &GlobalLibraryWriter, duration: Duration) -> GlobalPreparationReservation {
    writer
        .submit_preparation(request(writer, duration))
        .unwrap()
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap()
}
fn external(guard: &WritableProfileGuard) -> Connection {
    let connection = Connection::open(
        guard
            .profile_root()
            .join(super::super::super::LIBRARY_DB_FILE_NAME),
    )
    .unwrap();
    connection.busy_timeout(Duration::ZERO).unwrap();
    connection
}
fn release(reservation: GlobalPreparationReservation) {
    reservation
        .abort()
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap();
}
fn empty(writer: &GlobalLibraryWriter) {
    assert_eq!(
        writer
            .load_ui_preferences_payload()
            .unwrap()
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap(),
        None
    );
}

#[test]
fn preparation_holds_actual_writer_and_bounded_ordinary_queue_until_confirmed_abort() {
    fixture(|writer, guard, _| {
        let reservation = reserve(writer, Duration::from_secs(2));
        let observation = reservation.observation().clone();
        assert_eq!(observation.acquisition(), &writer.acquisition_id());
        assert_eq!(observation.command_digest(), &[7; 32]);
        assert!(!observation.database_identity().is_empty());
        let external = external(guard);
        assert!(matches!(external.execute_batch("BEGIN IMMEDIATE"),
            Err(rusqlite::Error::SqliteFailure(error,_)) if error.code == rusqlite::ErrorCode::DatabaseBusy));
        let mut queued = Vec::new();
        for index in 0..GLOBAL_LIBRARY_WRITER_COMMAND_CAPACITY {
            queued.push(
                writer
                    .replace_ui_preferences_payload(format!("queued-{index}"))
                    .unwrap(),
            );
        }
        assert!(matches!(
            writer.submit_preparation(request(writer, Duration::from_secs(1))),
            Err(GlobalLibraryWriterQueueError::Full)
        ));
        assert!(matches!(
            queued[0].try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        let unchanged: i64 = external
            .query_row(
                "SELECT count(*) FROM metadata WHERE key=?1",
                [super::super::super::UI_PREFERENCES_KEY],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unchanged, 0);
        release(reservation);
        for receiver in queued {
            receiver
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
        }
        external.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        assert_eq!(
            writer
                .load_ui_preferences_payload()
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap(),
            Some("queued-31".into())
        );
        let next = reserve(writer, Duration::from_secs(1));
        assert_ne!(next.observation().nonce(), observation.nonce());
        release(next);
    });
}

#[test]
fn preparation_busy_stale_and_bounds_do_not_change_metadata_or_context() {
    fixture(|writer, guard, _| {
        let external = external(guard);
        external.execute_batch("BEGIN IMMEDIATE").unwrap();
        let started = Instant::now();
        let error = writer
            .submit_preparation(request(writer, Duration::from_secs(1)))
            .unwrap()
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .err()
            .unwrap();
        assert_eq!(
            error,
            GlobalPreparationError::Acquisition {
                sqlite_code: rusqlite::ffi::SQLITE_BUSY
            }
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        external.execute_batch("ROLLBACK").unwrap();
        let stale = GlobalPreparationRequest::new(
            GlobalPreparationOwner::new(ProfileAcquisitionId::test_only()),
            [7; 32],
            Instant::now() + Duration::from_secs(1),
            1,
            128,
        )
        .unwrap();
        assert_eq!(
            writer
                .submit_preparation(stale)
                .unwrap()
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .err(),
            Some(GlobalPreparationError::StaleOwner)
        );
        for (count, bytes) in [
            (0, 128),
            (65, 128),
            (1, 0),
            (1, GLOBAL_PREPARATION_MAX_BYTES + 1),
        ] {
            assert_eq!(
                GlobalPreparationRequest::new(
                    writer.preparation_owner(),
                    [7; 32],
                    Instant::now() + Duration::from_secs(1),
                    count,
                    bytes
                )
                .err(),
                Some(GlobalPreparationError::InputBound)
            );
        }
        assert_eq!(
            GlobalPreparationRequest::new(
                writer.preparation_owner(),
                [7; 32],
                Instant::now() + Duration::from_secs(3),
                1,
                128
            )
            .err(),
            Some(GlobalPreparationError::InputBound)
        );
        empty(writer);
        release(reserve(writer, Duration::from_secs(1)));
        empty(writer);
    });
}

#[test]
fn preparation_dropped_delivery_canceled_queue_and_expiry_release_before_reuse() {
    fixture(|writer, guard, _| {
        let held = reserve(writer, Duration::from_secs(2));
        let mut expired = writer
            .submit_preparation(request(writer, Duration::from_millis(30)))
            .unwrap();
        let canceled = writer
            .submit_preparation(request(writer, Duration::from_secs(1)))
            .unwrap();
        drop(canceled);
        // The first reservation prevents queue dispatch until this expired request's deadline.
        assert!(matches!(
            expired.recv_timeout(Duration::from_millis(60)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        release(held);
        assert_eq!(
            expired.recv_timeout(Duration::from_secs(1)).unwrap().err(),
            Some(GlobalPreparationError::Deadline)
        );
        empty(writer);
        let mut held = reserve(writer, Duration::from_millis(50));
        let released = held.released.take(); // observe automatic deadline, without issuing abort
        assert_eq!(
            released
                .unwrap()
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            Err(GlobalPreparationError::Deadline)
        );
        drop(held);
        empty(writer);
        // Lost live handle and lost explicit release response still drive actual rollback.
        drop(reserve(writer, Duration::from_secs(1)));
        empty(writer);
        drop(reserve(writer, Duration::from_secs(1)).abort());
        empty(writer);
        // Drop the response before receiving it: cancellation remains owned by the worker.
        drop(
            writer
                .submit_preparation(request(writer, Duration::from_secs(1)))
                .unwrap(),
        );
        empty(writer);
        external(guard)
            .execute_batch("BEGIN IMMEDIATE; ROLLBACK")
            .unwrap();
    });
}

#[test]
fn preparation_shutdown_aborts_held_transaction_before_draining_accepted_work() {
    fixture(|writer, guard, _| {
        let mut held = reserve(writer, Duration::from_secs(2));
        let queued = writer
            .replace_ui_preferences_payload("accepted-before-close".into())
            .unwrap();
        let released = held.released.take().unwrap();
        writer.begin_shutdown();
        assert_eq!(
            released.recv_timeout(Duration::from_secs(1)).unwrap(),
            Err(GlobalPreparationError::Closing)
        );
        writer.shutdown_with_report();
        queued
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        drop(held);
        external(guard)
            .execute_batch("BEGIN IMMEDIATE; ROLLBACK")
            .unwrap();
        assert_eq!(writer.status(), GlobalLibraryWriterStatus::Closed);
    });
}

#[test]
fn preparation_real_hard_wal_rejects_without_mutating_preferences_or_context() {
    fixture(|writer, guard, _| {
        let reader = Connection::open_with_flags(
            guard
                .profile_root()
                .join(super::super::super::LIBRARY_DB_FILE_NAME),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        reader
            .execute_batch("BEGIN; SELECT count(*) FROM metadata;")
            .unwrap();
        // Actual bounded ordinary commits, not injected byte counts/capacity, grow retained WAL.
        let mut last = None;
        for n in 1..=1024 {
            let payload = format!(
                "{n:08}{}",
                if n % 2 == 0 { "x" } else { "y" }
                    .repeat(crate::sample_sources::UI_PREFERENCES_MAX_BYTES - 8)
            );
            match writer
                .replace_ui_preferences_payload(payload.clone())
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
            {
                Ok(()) => last = Some(payload),
                Err(LibraryError::WalAdmission(crate::WalAdmissionError::HardWatermark {
                    ..
                })) => break,
                other => panic!("{other:?}"),
            }
        }
        let last = last.unwrap();
        let error = writer
            .submit_preparation(request(writer, Duration::from_secs(1)))
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .err()
            .unwrap();
        assert!(
            matches!(error,GlobalPreparationError::Wal(crate::WalAdmissionError::HardWatermark{observed_bytes,..}) if observed_bytes>=crate::WAL_HARD_WATERMARK_BYTES)
        );
        assert_eq!(
            writer
                .load_ui_preferences_payload()
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap(),
            Some(last)
        );
        assert_eq!(
            reader
                .query_row(
                    "SELECT count(*) FROM metadata WHERE key=?1",
                    [super::super::super::UI_PREFERENCES_KEY],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        reader.execute_batch("ROLLBACK").unwrap();
        // Real external maintenance fixture; production preparation never checkpoints.
        external(guard)
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        release(reserve(writer, Duration::from_secs(1)));
    });
}

#[cfg(unix)]
#[test]
fn preparation_actual_main_replacement_retires_connection_without_queued_effect() {
    fixture(|writer, guard, temp| {
        let mut held = reserve(writer, Duration::from_secs(2));
        let queued = writer
            .replace_ui_preferences_payload("must-not-commit".into())
            .unwrap();
        let released = held.released.take().unwrap();
        let path = guard
            .profile_root()
            .join(super::super::super::LIBRARY_DB_FILE_NAME);
        let old = temp.path().join("old-main");
        std::fs::rename(&path, &old).unwrap();
        std::fs::write(&path, []).unwrap();
        assert_eq!(
            released.recv_timeout(Duration::from_secs(1)).unwrap(),
            Err(GlobalPreparationError::DatabaseBinding)
        );
        assert!(
            queued
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .is_err()
        );
        writer.shutdown_with_report();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        // Restore after retained connection close to leave SQLite teardown coherent.
        std::fs::remove_file(&path).unwrap();
        std::fs::rename(old, path).unwrap();
        drop(held);
    });
}

#[test]
fn preparation_retained_unknown_mode_and_rollback_denial_require_no_fake_completion() {
    fixture(|writer, guard, _| {
        writer.shutdown_with_report();
        let database = LibraryDatabase::open_for_profile_guard(guard, None).unwrap();
        database
            .connection
            .execute_batch("PRAGMA journal_mode=DELETE")
            .unwrap();
        let (ready, receiver) = mpsc::sync_channel(1);
        let command = Command {
            request: request(writer, Duration::from_secs(1)),
            canceled: Arc::new(AtomicBool::new(false)),
            ready,
        };
        let lifecycle = std::sync::atomic::AtomicU8::new(WriterLifecycle::Available as u8);
        assert!(run(
            command,
            &database,
            guard,
            &lifecycle,
            writer.preparation_owner().registration(),
            &mutation_fence::MutationFence::new()
        ));
        assert!(matches!(
            receiver.recv().unwrap(),
            Err(GlobalPreparationError::Wal(
                crate::WalAdmissionError::ObservationUnavailable { .. }
            ))
        ));
        assert!(database.connection.is_autocommit());
        let timeout: u64 = database
            .connection
            .pragma_query_value(None, "busy_timeout", |r| r.get(0))
            .unwrap();
        database
            .connection
            .execute_batch("PRAGMA journal_mode=WAL; BEGIN IMMEDIATE; ROLLBACK")
            .unwrap();
        database
            .connection
            .authorizer(Some(|ctx: rusqlite::hooks::AuthContext<'_>| {
                if matches!(
                    ctx.action,
                    rusqlite::hooks::AuthAction::Transaction {
                        operation: rusqlite::hooks::TransactionOperation::Rollback
                    }
                ) {
                    rusqlite::hooks::Authorization::Deny
                } else {
                    rusqlite::hooks::Authorization::Allow
                }
            }));
        let (ready, receiver) = mpsc::sync_channel(1);
        let command = Command {
            request: request(writer, Duration::from_secs(1)),
            canceled: Arc::new(AtomicBool::new(false)),
            ready,
        };
        let client = std::thread::spawn(move || {
            receiver
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap()
                .abort()
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
        });
        assert!(!run(
            command,
            &database,
            guard,
            &lifecycle,
            writer.preparation_owner().registration(),
            &mutation_fence::MutationFence::new()
        ));
        assert_eq!(
            client.join().unwrap(),
            Err(GlobalPreparationError::ConnectionRetired)
        );
        assert!(!database.connection.is_autocommit());
        assert_eq!(
            database
                .connection
                .pragma_query_value(None, "busy_timeout", |r| r.get::<_, u64>(0))
                .unwrap(),
            timeout
        );
        database.connection.authorizer(
            None::<fn(rusqlite::hooks::AuthContext<'_>) -> rusqlite::hooks::Authorization>,
        );
        database.connection.execute_batch("ROLLBACK").unwrap();
        assert_eq!(
            database
                .connection
                .query_row(
                    "SELECT count(*) FROM metadata WHERE key=?1",
                    [super::super::super::UI_PREFERENCES_KEY],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
    });
}

#[test]
fn preparation_restart_registration_rejects_delayed_same_profile_request() {
    fixture(|writer, guard, _| {
        let old_owner = writer.preparation_owner();
        let delayed = request(writer, Duration::from_secs(2));
        writer.shutdown_with_report();
        let mut replacement = GlobalLibraryWriter::start(guard).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while replacement.status() != GlobalLibraryWriterStatus::Available {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            old_owner.acquisition(),
            replacement.preparation_owner().acquisition()
        );
        assert_ne!(
            old_owner.registration(),
            replacement.preparation_owner().registration()
        );
        assert_eq!(
            replacement
                .submit_preparation(delayed)
                .unwrap()
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .err(),
            Some(GlobalPreparationError::StaleOwner)
        );
        empty(&replacement);
        release(reserve(&replacement, Duration::from_secs(1)));
        replacement.shutdown_with_report();
    });
}

#[cfg(unix)]
#[test]
fn preparation_profile_loss_releases_without_dispatching_accepted_mutation() {
    fixture(|writer, guard, temp| {
        let mut held = reserve(writer, Duration::from_secs(2));
        let queued = writer
            .replace_ui_preferences_payload("must-not-commit".into())
            .unwrap();
        let released = held.released.take().unwrap();
        let root = guard.profile_root();
        let displaced = temp.path().join("old-profile");
        std::fs::rename(root, &displaced).unwrap();
        std::fs::create_dir(root).unwrap();
        assert_eq!(
            released.recv_timeout(Duration::from_secs(1)).unwrap(),
            Err(GlobalPreparationError::StaleOwner)
        );
        assert!(matches!(
            queued.recv_timeout(Duration::from_secs(1)).unwrap(),
            Err(LibraryError::ProfileOwnershipChanged { .. })
        ));
        writer.shutdown_with_report();
        assert!(
            !root
                .join(super::super::super::LIBRARY_DB_FILE_NAME)
                .exists()
        );
        let old =
            Connection::open(displaced.join(super::super::super::LIBRARY_DB_FILE_NAME)).unwrap();
        assert_eq!(
            old.query_row(
                "SELECT count(*) FROM metadata WHERE key=?1",
                [super::super::super::UI_PREFERENCES_KEY],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        drop(held);
    });
}

#[test]
fn preparation_one_actual_begin_and_timeout_restore_with_no_row_or_checkpoint_sql() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
    use std::sync::atomic::AtomicUsize;
    fixture(|writer, guard, _| {
        writer.shutdown_with_report();
        let database = LibraryDatabase::open_for_profile_guard(guard, None).unwrap();
        database
            .connection
            .busy_timeout(Duration::from_millis(731))
            .unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&attempts);
        database
            .connection
            .authorizer(Some(move |ctx: AuthContext<'_>| match ctx.action {
                AuthAction::Transaction {
                    operation: TransactionOperation::Begin,
                } => {
                    seen.fetch_add(1, Ordering::SeqCst);
                    Authorization::Allow
                }
                AuthAction::Read { .. }
                | AuthAction::Insert { .. }
                | AuthAction::Update { .. }
                | AuthAction::Delete { .. }
                | AuthAction::Pragma {
                    pragma_name: "wal_checkpoint",
                    ..
                } => Authorization::Deny,
                _ => Authorization::Allow,
            }));
        let external = external(guard);
        external.execute_batch("BEGIN IMMEDIATE").unwrap();
        let (ready, receiver) = mpsc::sync_channel(1);
        let command = Command {
            request: request(writer, Duration::from_secs(1)),
            canceled: Arc::new(AtomicBool::new(false)),
            ready,
        };
        let lifecycle = std::sync::atomic::AtomicU8::new(WriterLifecycle::Available as u8);
        assert!(run(
            command,
            &database,
            guard,
            &lifecycle,
            writer.preparation_owner().registration(),
            &mutation_fence::MutationFence::new()
        ));
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(1)).unwrap().err(),
            Some(GlobalPreparationError::Acquisition {
                sqlite_code: rusqlite::ffi::SQLITE_BUSY
            })
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(
            database
                .connection
                .pragma_query_value(None, "busy_timeout", |r| r.get::<_, u64>(0))
                .unwrap(),
            731
        );
        external.execute_batch("ROLLBACK").unwrap();
        let (ready, receiver) = mpsc::sync_channel(1);
        let command = Command {
            request: request(writer, Duration::from_secs(1)),
            canceled: Arc::new(AtomicBool::new(false)),
            ready,
        };
        let client = std::thread::spawn(move || {
            receiver
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap()
                .abort()
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
        });
        assert!(run(
            command,
            &database,
            guard,
            &lifecycle,
            writer.preparation_owner().registration(),
            &mutation_fence::MutationFence::new()
        ));
        client.join().unwrap().unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(
            database
                .connection
                .pragma_query_value(None, "busy_timeout", |r| r.get::<_, u64>(0))
                .unwrap(),
            731
        );
        assert!(database.connection.is_autocommit());
    });
}
