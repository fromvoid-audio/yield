use super::*;

#[cfg(unix)]
#[test]
fn idle_global_wal_actual_worker_profile_replacement_before_or_during_pass_closes() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    for during in [false, true] {
        let _profile = PersistenceProfileGuard::named(format!("idle-profile-{during}"));
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let root = guard.profile_root().to_path_buf();
        let moved = root.with_extension("retained-idle");
        let counter = Arc::new(AtomicUsize::new(0));
        let pressure = pressure_hook(counter.clone(), Fault::None);
        let hook_root = root.clone();
        let hook_moved = moved.clone();
        let hook_counter = counter.clone();
        let mut writer = GlobalLibraryWriter::start_with_database_hook_for_test(
            &guard,
            Arc::new(move |connection| {
                pressure(connection);
                if during {
                    let root = hook_root.clone();
                    let moved = hook_moved.clone();
                    let counter = hook_counter.clone();
                    connection.authorizer(Some(move |context: AuthContext<'_>| {
                        if matches!(
                            context.action,
                            AuthAction::Pragma {
                                pragma_name: "wal_checkpoint",
                                ..
                            }
                        ) && counter.fetch_add(1, Ordering::SeqCst) == 0
                        {
                            std::fs::rename(&root, &moved).unwrap();
                            std::fs::create_dir(&root).unwrap();
                        }
                        Authorization::Allow
                    }));
                }
            }),
            None,
        )
        .unwrap();
        available(&writer);
        if during {
            await_idle(&counter);
        } else {
            std::fs::rename(&root, &moved).unwrap();
            std::fs::create_dir(&root).unwrap();
        }
        closed_after_profile_change(&writer);
        assert_eq!(counter.load(Ordering::SeqCst), usize::from(during));
        assert!(!root.join(LIBRARY_DB_FILE_NAME).exists());
        let retained = Connection::open_with_flags(
            moved.join(LIBRARY_DB_FILE_NAME),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        assert_eq!(
            retained
                .query_row("SELECT length(value) FROM maintenance_payload", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            34_603_008
        );
        let _ = writer.shutdown();
    }
}

#[test]
fn idle_global_wal_actual_worker_shutdown_does_not_wait_for_checkpoint_admission_gate() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    let _profile = PersistenceProfileGuard::named("idle-admitted-shutdown");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let counter = Arc::new(AtomicUsize::new(0));
    let pressure = pressure_hook(counter.clone(), Fault::None);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release = Arc::new(std::sync::Mutex::new(release_rx));
    let hook_counter = counter.clone();
    let mut writer = GlobalLibraryWriter::start_with_database_hook_for_test(
        &guard,
        Arc::new(move |connection| {
            pressure(connection);
            let ready = ready_tx.clone();
            let release = release.clone();
            let counter = hook_counter.clone();
            connection.authorizer(Some(move |context: AuthContext<'_>| {
                if matches!(
                    context.action,
                    AuthAction::Pragma {
                        pragma_name: "wal_checkpoint",
                        ..
                    }
                ) {
                    counter.fetch_add(1, Ordering::SeqCst);
                    ready.send(()).unwrap();
                    release
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(3))
                        .unwrap();
                }
                Authorization::Allow
            }));
        }),
        None,
    )
    .unwrap();
    available(&writer);
    ready_rx.recv_timeout(Duration::from_secs(18)).unwrap();
    let started = Instant::now();
    assert!(writer.begin_shutdown());
    let admission_time = started.elapsed();
    assert_eq!(writer.status(), GlobalLibraryWriterStatus::Closing);
    release_tx.send(()).unwrap();
    assert!(
        admission_time < Duration::from_secs(1),
        "shutdown held behind gated checkpoint"
    );
    let report = writer.shutdown_with_report();
    assert!(matches!(
        report.outcome(),
        GlobalLibraryWriterDrainOutcome::Drained
    ));
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    assert!(matches!(
        writer.load_ui_preferences_payload(),
        Err(GlobalLibraryWriterQueueError::Closed)
    ));
}
