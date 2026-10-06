//! Real idle worker pressure; external SQLite writes are fixture setup only.
use super::*;
use yield_library::sample_sources::db::DB_FILE_NAME;

fn start_source_idle(
    fixture: &tests::Fixture,
    configure: impl FnOnce(&CatalogSourceDatabase, &rusqlite::Connection) + Send + 'static,
) -> (CatalogService, rusqlite::Connection, PathBuf, u64) {
    let source_id = SourceId::from_string("idle-original");
    let guard = fixture.guard.try_clone().unwrap();
    let root = fixture.root.path().to_path_buf();
    let main = guard
        .profile_root()
        .join(catalog_profile_child(&source_id))
        .join(DB_FILE_NAME);
    let admission = Arc::new(Mutex::new(AdmissionState::new()));
    let events = Arc::new(EventHub::new());
    let (commands, receiver) = mpsc::sync_channel(COMMAND_CAPACITY);
    let (ready, startup) = mpsc::sync_channel(1);
    let worker_admission = admission.clone();
    let worker_events = events.clone();
    let worker_id = source_id.clone();
    let worker_main = main.clone();
    let worker = thread::spawn(move || {
        let database = SourceDatabase::open_for_catalog_with_profile_guard(
            &root,
            &guard,
            catalog_profile_child(&worker_id),
        )
        .unwrap();
        let publication_fence = database.source_publication_fence().unwrap();
        let database = CatalogSourceDatabase::new(database, worker_id.clone(), 1).unwrap();
        let external = rusqlite::Connection::open(&worker_main).unwrap();
        configure(&database, &external);
        external
            .execute_batch(
                "PRAGMA wal_autocheckpoint=0;
                 CREATE TABLE idle_payload(value BLOB);
                 INSERT INTO idle_payload VALUES (zeroblob(34603008));",
            )
            .unwrap();
        let before = fs::metadata(&worker_main).unwrap().len();
        assert!(before < 34603008);
        worker_admission.lock().unwrap().sources.insert(
            worker_id.clone(),
            DesiredSource {
                lifecycle_generation: 1,
                active: true,
                attempt_token: None,
                last_curation_request_id: None,
            },
        );
        let mut sources = HashMap::new();
        sources.insert(
            worker_id,
            RegisteredSource {
                collection_history_token: None,
                replay: None,
                root,
                lifecycle_generation: 1,
                database,
                publication_fence,
                known_root_identity: None,
                snapshot: None,
                attempt_token: None,
                curation_lineage: None,
                watcher: None,
                watcher_schedule: WatcherSchedule::new(false),
                source_authority: None,
                source_authority_receipt: None,
                source_authority_pending: None,
            },
        );
        ready.send((external, before)).unwrap();
        run_worker_with_sources(
            guard,
            receiver,
            worker_events,
            worker_admission,
            None,
            None,
            WatcherMode::Detached,
            None,
            sources,
        );
    });
    let service = CatalogService {
        commands,
        events,
        admission,
        worker: Mutex::new(WorkerJoinState::new(Some(worker))),
    };
    let (external, before) = startup.recv_timeout(Duration::from_secs(5)).unwrap();
    (service, external, main, before)
}

#[test]
fn original_source_idle_worker_checkpoints_real_wal_pressure() {
    let fixture = tests::Fixture::new("source-idle-wal");
    let (service, external, main, before) = start_source_idle(&fixture, |_, _| {});
    let started = Instant::now();
    let deadline = started + Duration::from_secs(18);
    while fs::metadata(&main).unwrap().len() < 34603008 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
    let after = fs::metadata(&main).unwrap().len();
    // Always join the real owner, including the pre-implementation failure path.
    service.shutdown();
    assert!(
        after >= 34603008,
        "original idle worker did not checkpoint pressure: MAIN {before} -> {after}"
    );
    assert!(started.elapsed() >= Duration::from_secs(15));
    assert_eq!(
        external
            .query_row("SELECT length(value) FROM idle_payload", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        34603008
    );
}

#[test]
fn original_source_idle_worker_preserves_reader_and_retries_only_on_next_idle_opportunity() {
    let fixture = tests::Fixture::new("source-idle-reader");
    let (send, receive) = mpsc::sync_channel(1);
    let (service, external, main, before) = start_source_idle(&fixture, move |_, external| {
        let reader = rusqlite::Connection::open(external.path().unwrap()).unwrap();
        reader
            .execute_batch("BEGIN; SELECT singleton FROM source_lease")
            .unwrap();
        send.send(reader).unwrap();
    });
    let reader = receive.recv_timeout(Duration::from_secs(1)).unwrap();
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(16) {
        thread::sleep(Duration::from_millis(25));
    }
    let partial = fs::metadata(&main).unwrap().len();
    assert!(
        partial < 34603008,
        "pinned original reader was bypassed: {before} -> {partial}"
    );
    reader.execute_batch("ROLLBACK").unwrap();
    let deadline = started + Duration::from_secs(34);
    while fs::metadata(&main).unwrap().len() < 34603008 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
    let after = fs::metadata(&main).unwrap().len();
    service.shutdown();
    assert!(
        after >= 34603008,
        "next real idle opportunity did not checkpoint released reader"
    );
    assert!(started.elapsed() >= Duration::from_secs(30));
    assert_eq!(
        external
            .query_row("SELECT length(value) FROM idle_payload", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        34603008
    );
}

#[test]
fn original_source_idle_worker_closing_joins_an_admitted_native_checkpoint() {
    let fixture = tests::Fixture::new("source-idle-closing");
    let (entered, seen) = mpsc::sync_channel(1);
    let (release, resume) = mpsc::sync_channel(1);
    let (service, external, main, _) = start_source_idle(&fixture, move |database, _| {
        database
            .writer
            .block_idle_wal_checkpoint_for_tests(entered, resume);
    });
    let entered = seen.recv_timeout(Duration::from_secs(20));
    let service = Arc::new(service);
    if entered.is_err() {
        // Release also makes failure teardown safe if the original owner arrives late.
        let _ = release.send(());
        service.shutdown();
        panic!("original native checkpoint never admitted: {entered:?}");
    }
    let (done, joined) = mpsc::sync_channel(1);
    let closing = service.clone();
    let shutdown = thread::spawn(move || {
        closing.shutdown();
        done.send(()).unwrap();
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    while !admission_state(&service.admission, |state| state.shutdown) {
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
    assert!(matches!(
        joined.recv_timeout(Duration::from_millis(50)),
        Err(RecvTimeoutError::Timeout)
    ));
    assert!(matches!(
        WritableProfileGuard::acquire_current(),
        Err(ProfileOwnershipError::ProfileOwnedByAnotherProcess { .. })
    ));
    let id = SourceId::from_string("idle-original");
    assert!(matches!(
        SourceDatabase::open_for_catalog_with_profile_guard(
            fixture.root.path(),
            &fixture.guard,
            catalog_profile_child(&id),
        ),
        Err(SourceDbError::ProfileOwnership(
            ProfileOwnershipError::ProfileChildAlreadyLeased { .. }
        ))
    ));
    // Admission mutex remains available while the actual Native/SQLite pass is paused.
    assert!(service.admission.try_lock().is_ok());
    assert!(matches!(
        service.submit(CatalogCommand::RequestAudit {
            source_id: SourceId::from_string("idle-original"),
            lifecycle_generation: 1,
        }),
        Err(CatalogSubmitError::Closed)
    ));
    release.send(()).unwrap();
    joined.recv_timeout(Duration::from_secs(5)).unwrap();
    shutdown.join().unwrap();
    let reopened = SourceDatabase::open_for_catalog_with_profile_guard(
        fixture.root.path(),
        &fixture.guard,
        catalog_profile_child(&id),
    )
    .unwrap();
    assert!(reopened.validate_profile_guard(&fixture.guard).is_ok());
    assert!(fs::metadata(main).unwrap().len() >= 34603008);
    assert_eq!(
        external
            .query_row("SELECT length(value) FROM idle_payload", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        34603008
    );
}
