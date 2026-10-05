//! Real global worker and original owned SQLite cleanup tests; no admitted effect fixtures.
#[cfg(all(test, target_os = "linux"))]
mod integration {
    use super::super::tests::{external, fixture, release, request};
    use super::super::*;
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
    use std::sync::atomic::{AtomicU8, AtomicUsize};
    fn settings(database: &LibraryDatabase) -> [u64; 6] {
        [
            "max_page_count",
            "temp_store",
            "cache_spill",
            "wal_autocheckpoint",
            "mmap_size",
            "busy_timeout",
        ]
        .map(|name| {
            database
                .connection
                .pragma_query_value(None, name, |r| r.get(0))
                .unwrap()
        })
    }
    fn direct_command(
        writer: &GlobalLibraryWriter,
    ) -> (
        Command,
        Receiver<Result<GlobalPreparationReservation, GlobalPreparationError>>,
    ) {
        let (ready, receiver) = mpsc::sync_channel(1);
        (
            Command {
                request: request(writer, Duration::from_secs(2)).with_lifecycle_policy(),
                canceled: Arc::new(AtomicBool::new(false)),
                ready,
                binding_credit: Some(writer.preparation_binding_budget.reserve().unwrap()),
                handshake_credit: None,
            },
            receiver,
        )
    }
    #[test]
    fn lifecycle_policy_actual_worker_holds_external_and_queued_writes_then_restores() {
        fixture(|writer, guard, _| {
            let external = external(guard);
            let before = writer
                .submit_preparation(
                    request(writer, Duration::from_secs(2)).with_lifecycle_allocation(),
                )
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
            let original = before.observation().lifecycle_allocation().unwrap().clone();
            assert!(!before.observation().lifecycle_policy_was_entered());
            release(before);
            let held = writer
                .submit_preparation(request(writer, Duration::from_secs(2)).with_lifecycle_policy())
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
            assert!(held.observation().lifecycle_policy_was_entered());
            assert!(held.observation().wal_scope().is_some());
            assert_eq!(held.observation().lifecycle_allocation(), Some(&original));
            assert_eq!(
                external
                    .execute_batch("BEGIN IMMEDIATE")
                    .unwrap_err()
                    .sqlite_error_code(),
                Some(rusqlite::ErrorCode::DatabaseBusy)
            );
            let queued = writer
                .replace_ui_preferences_payload(r#"{"policy":"after-abort"}"#.into())
                .unwrap();
            assert!(matches!(queued.try_recv(), Err(mpsc::TryRecvError::Empty)));
            release(held);
            queued
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
            external.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
            assert!(writer.shutdown());
        });
    }
    #[test]
    fn lifecycle_policy_actual_deadline_drop_and_shutdown_release_without_effect() {
        for mode in 0..3 {
            fixture(|writer, guard, _| {
                let external = external(guard);
                let mut pending = writer
                    .submit_preparation(
                        request(writer, Duration::from_millis(200)).with_lifecycle_policy(),
                    )
                    .unwrap();
                let held = pending
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .unwrap();
                assert!(held.observation().lifecycle_policy_was_entered());
                let queued = writer.load_ui_preferences_payload().unwrap();
                match mode {
                    0 => {
                        std::thread::sleep(Duration::from_millis(250));
                        assert_eq!(
                            held.abort().recv_timeout(Duration::from_secs(3)).unwrap(),
                            Err(GlobalPreparationError::Deadline)
                        );
                        assert_eq!(
                            queued
                                .recv_timeout(Duration::from_secs(3))
                                .unwrap()
                                .unwrap(),
                            None
                        );
                        assert!(writer.shutdown());
                    }
                    1 => {
                        drop(held);
                        assert_eq!(
                            queued
                                .recv_timeout(Duration::from_secs(3))
                                .unwrap()
                                .unwrap(),
                            None
                        );
                        assert!(writer.shutdown());
                    }
                    _ => {
                        assert!(writer.shutdown());
                        assert_eq!(
                            queued
                                .recv_timeout(Duration::from_secs(3))
                                .unwrap()
                                .unwrap(),
                            None
                        );
                        assert_eq!(
                            held.abort().recv_timeout(Duration::from_secs(3)).unwrap(),
                            Err(GlobalPreparationError::Closing)
                        );
                    }
                }
                external.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
            });
        }
    }
    #[test]
    fn lifecycle_policy_actual_busy_and_lost_delivery_restore_exact_settings() {
        fixture(|writer, guard, _| {
            writer.shutdown_with_report();
            let database = LibraryDatabase::open_for_profile_guard(guard, None).unwrap();
            database.connection.execute_batch("PRAGMA temp_store=FILE; PRAGMA wal_autocheckpoint=73; PRAGMA mmap_size=1048576").unwrap();
            database
                .connection
                .busy_timeout(Duration::from_millis(917))
                .unwrap();
            let original = settings(&database);
            let attempts = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&attempts);
            database
                .connection
                .authorizer(Some(move |ctx: AuthContext<'_>| {
                    if matches!(
                        ctx.action,
                        AuthAction::Transaction {
                            operation: TransactionOperation::Begin
                        }
                    ) {
                        observed.fetch_add(1, Ordering::SeqCst);
                    }
                    Authorization::Allow
                }));
            let external = external(guard);
            external.execute_batch("BEGIN IMMEDIATE").unwrap();
            let (command, receiver) = direct_command(writer);
            let lifecycle = AtomicU8::new(WriterLifecycle::Available as u8);
            assert!(run(
                command,
                &database,
                guard,
                &lifecycle,
                writer.preparation_owner().registration(),
                writer.preparation_owner().generation(),
                &mutation_fence::MutationFence::new()
            ));
            assert!(matches!(
                receiver.recv().unwrap(),
                Err(GlobalPreparationError::Acquisition {
                    sqlite_code: rusqlite::ffi::SQLITE_BUSY
                })
            ));
            assert_eq!(settings(&database), original);
            external.execute_batch("ROLLBACK").unwrap();
            let (command, receiver) = direct_command(writer);
            drop(receiver);
            assert!(run(
                command,
                &database,
                guard,
                &lifecycle,
                writer.preparation_owner().registration(),
                writer.preparation_owner().generation(),
                &mutation_fence::MutationFence::new()
            ));
            assert_eq!(
                attempts.load(Ordering::SeqCst),
                2,
                "one native BEGIN per request"
            );
            assert!(database.connection.is_autocommit());
            assert_eq!(settings(&database), original);
        });
    }
    #[test]
    fn lifecycle_policy_actual_worker_denies_release_ack_on_rollback_or_restoration_failure() {
        for rollback_denied in [true, false] {
            fixture(|writer, guard, _| {
                writer.shutdown_with_report();
                let database = LibraryDatabase::open_for_profile_guard(guard, None).unwrap();
                let acquired = Arc::new(AtomicBool::new(false));
                let seen = Arc::clone(&acquired);
                database
                    .connection
                    .authorizer(Some(move |ctx: AuthContext<'_>| match ctx.action {
                        AuthAction::Transaction {
                            operation: TransactionOperation::Begin,
                        } => {
                            seen.store(true, Ordering::Release);
                            Authorization::Allow
                        }
                        AuthAction::Transaction {
                            operation: TransactionOperation::Rollback,
                        } if rollback_denied => Authorization::Deny,
                        AuthAction::Pragma {
                            pragma_name: "temp_store",
                            pragma_value: Some(_),
                        } if !rollback_denied && seen.load(Ordering::Acquire) => {
                            Authorization::Deny
                        }
                        _ => Authorization::Allow,
                    }));
                let (command, receiver) = direct_command(writer);
                let client = std::thread::spawn(move || {
                    receiver
                        .recv_timeout(Duration::from_secs(3))
                        .unwrap()
                        .unwrap()
                        .abort()
                        .recv_timeout(Duration::from_secs(3))
                        .unwrap()
                });
                let lifecycle = AtomicU8::new(WriterLifecycle::Available as u8);
                assert!(!run(
                    command,
                    &database,
                    guard,
                    &lifecycle,
                    writer.preparation_owner().registration(),
                    writer.preparation_owner().generation(),
                    &mutation_fence::MutationFence::new()
                ));
                assert_eq!(
                    client.join().unwrap(),
                    Err(GlobalPreparationError::ConnectionRetired)
                );
                assert!(
                    database
                        .connection
                        .pragma_query_value(None, "query_only", |r| r.get::<_, bool>(0))
                        .unwrap()
                );
                database
                    .connection
                    .authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
                if !database.connection.is_autocommit() {
                    database.connection.execute_batch("ROLLBACK").unwrap();
                }
            });
        }
    }
    #[test]
    fn lifecycle_policy_cancel_wins_native_poll_but_final_binding_refuses_sidecar_reuse() {
        fixture(|writer, guard, _| {
            writer.shutdown_with_report();
            let database = LibraryDatabase::open_for_profile_guard(guard, None).unwrap();
            let parked = Arc::new(std::sync::Barrier::new(2));
            let gate = Arc::clone(&parked);
            let armed = Arc::new(AtomicBool::new(false));
            let observed = Arc::clone(&armed);
            database
                .connection
                .authorizer(Some(move |ctx: AuthContext<'_>| {
                    if matches!(
                        ctx.action,
                        AuthAction::Pragma {
                            pragma_name: "cache_spill",
                            pragma_value: None
                        }
                    ) && observed.swap(false, Ordering::SeqCst)
                    {
                        gate.wait();
                        gate.wait();
                    }
                    Authorization::Allow
                }));
            let (command, receiver) = direct_command(writer);
            let wal = guard.profile_root().join(format!(
                "{}-wal",
                super::super::super::super::LIBRARY_DB_FILE_NAME
            ));
            let client = std::thread::spawn(move || {
                let held = receiver
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .unwrap();
                armed.store(true, Ordering::SeqCst);
                parked.wait();
                let original = wal.with_extension("original-policy-wal");
                std::fs::rename(&wal, &original).unwrap();
                std::fs::copy(&original, &wal).unwrap();
                let released = held.abort();
                parked.wait();
                released.recv_timeout(Duration::from_secs(3)).unwrap()
            });
            let lifecycle = AtomicU8::new(WriterLifecycle::Available as u8);
            assert!(!run(
                command,
                &database,
                guard,
                &lifecycle,
                writer.preparation_owner().registration(),
                writer.preparation_owner().generation(),
                &mutation_fence::MutationFence::new()
            ));
            assert_eq!(
                client.join().unwrap(),
                Err(GlobalPreparationError::DatabaseBinding)
            );
            assert!(database.connection.is_autocommit());
        });
    }
}
