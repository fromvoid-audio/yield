use super::*;

mod ownership;

fn await_idle(observed: &AtomicUsize) {
    let started = Instant::now();
    while observed.load(Ordering::SeqCst) == 0 {
        assert!(
            started.elapsed() < Duration::from_secs(18),
            "idle owner never attempted maintenance"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn idle_global_wal_actual_worker_checkpoints_without_new_command_and_preserves_reader() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    let _profile = PersistenceProfileGuard::named("idle-maintenance-reader");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let counter = Arc::new(AtomicUsize::new(0));
    let mut writer = GlobalLibraryWriter::start_with_database_hook_for_test(
        &guard,
        pressure_hook(counter.clone(), Fault::None),
        None,
    )
    .unwrap();
    available(&writer);
    let reader = Connection::open_with_flags(
        guard.profile_root().join(LIBRARY_DB_FILE_NAME),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    reader
        .execute_batch("BEGIN; SELECT count(*) FROM metadata")
        .unwrap();
    let revision = writer.observed_mutation_revision_for_test();
    let started = Instant::now();
    while counter.load(Ordering::SeqCst) == 0 {
        assert!(
            started.elapsed() < Duration::from_secs(18),
            "idle owner never checkpointed"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        started.elapsed() >= Duration::from_secs(14),
        "production idle pacing was bypassed"
    );
    assert_eq!(writer.observed_mutation_revision_for_test(), revision);
    complete(
        &writer,
        GlobalMutationRequest::preferences(writer.acquisition_id(), 1, "after-idle".into())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "shared checkpoint throttle"
    );
    assert_eq!(
        reader
            .query_row(
                "SELECT count(*) FROM metadata WHERE key=?1",
                [UI_PREFERENCES_KEY],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    reader.execute_batch("ROLLBACK").unwrap();
    assert_eq!(
        reader
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [UI_PREFERENCES_KEY],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "after-idle"
    );
    drop(reader);
    assert!(writer.shutdown());
}

#[test]
fn idle_global_wal_actual_worker_refusals_preserve_availability_and_later_commit() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    for unknown in [false, true] {
        let _profile = PersistenceProfileGuard::named(format!("idle-refusal-{unknown}"));
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let counter = Arc::new(AtomicUsize::new(0));
        let observed = if unknown {
            Arc::new(AtomicUsize::new(0))
        } else {
            counter.clone()
        };
        let deny = Arc::new(AtomicBool::new(true));
        let pressure = pressure_hook(counter.clone(), Fault::CheckpointDenied);
        let hook_observed = observed.clone();
        let hook_deny = deny.clone();
        let hook_counter = counter.clone();
        let mut writer = GlobalLibraryWriter::start_with_database_hook_for_test(
            &guard,
            Arc::new(move |connection| {
                pressure(connection);
                if unknown {
                    let observed = hook_observed.clone();
                    let deny = hook_deny.clone();
                    let counter = hook_counter.clone();
                    connection.authorizer(Some(move |context: AuthContext<'_>| {
                        match context.action {
                            AuthAction::Pragma {
                                pragma_name: "journal_mode",
                                ..
                            } if deny.load(Ordering::SeqCst) => {
                                observed.fetch_add(1, Ordering::SeqCst);
                                Authorization::Deny
                            }
                            AuthAction::Pragma {
                                pragma_name: "wal_checkpoint",
                                ..
                            } => {
                                counter.fetch_add(1, Ordering::SeqCst);
                                Authorization::Allow
                            }
                            _ => Authorization::Allow,
                        }
                    }));
                }
            }),
            None,
        )
        .unwrap();
        available(&writer);
        await_idle(&observed);
        assert_eq!(writer.status(), GlobalLibraryWriterStatus::Available);
        assert!(
            writer
                .load_ui_preferences_payload()
                .unwrap()
                .recv()
                .unwrap()
                .unwrap()
                .is_none()
        );
        assert_eq!(counter.load(Ordering::SeqCst), usize::from(!unknown));
        deny.store(false, Ordering::SeqCst);
        complete(
            &writer,
            GlobalMutationRequest::preferences(writer.acquisition_id(), 1, "recovered".into())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert_eq!(
            writer
                .load_ui_preferences_payload()
                .unwrap()
                .recv()
                .unwrap()
                .unwrap()
                .as_deref(),
            Some("recovered")
        );
        assert!(writer.shutdown());
    }
}

#[test]
fn idle_global_wal_original_database_excludes_active_transaction_and_preserves_commit_marker() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    let _profile = PersistenceProfileGuard::named("idle-direct-transaction");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let database = LibraryDatabase::open_for_profile_guard(&guard, None).unwrap();
    let counter = Arc::new(AtomicUsize::new(0));
    pressure_hook(counter.clone(), Fault::None)(&database.connection);
    database.note_ordinary_commit();
    database
        .connection
        .execute_batch("BEGIN; SELECT count(*) FROM metadata")
        .unwrap();
    database.maintain_while_idle(&guard).unwrap();
    assert_eq!(counter.load(Ordering::SeqCst), 0);
    assert!(!database.ordinary_wal_maintenance.policy.has_attempted());
    assert!(database.ordinary_commit_pending_for_test());
    database.connection.execute_batch("ROLLBACK").unwrap();
    database.maintain_while_idle(&guard).unwrap();
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    assert!(database.ordinary_commit_pending_for_test());
    database.maintain_after_ordinary_commit(&guard);
    assert!(!database.ordinary_commit_pending_for_test());
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}
