//! Real idle worker pressure; external SQLite writes are fixture setup only.
use super::*;
use yield_library::sample_sources::db::DB_FILE_NAME;

#[test]
fn original_source_idle_worker_checkpoints_real_wal_pressure() {
    let fixture = tests::Fixture::new("source-idle-wal");
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
