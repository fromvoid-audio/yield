use super::*;

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
