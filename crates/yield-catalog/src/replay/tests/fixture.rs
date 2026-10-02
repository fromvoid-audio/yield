use super::*;

pub(super) struct Fixture {
    pub(super) base: crate::tests::Fixture,
    bootstrap: bool,
    pub(super) fail_replay_update: bool,
    pub(super) fail_uncertainty_commit: bool,
}
impl Fixture {
    pub(super) fn new(bootstrap: bool) -> Self {
        let f = Self {
            base: crate::tests::Fixture::new("catalog-production-replay"),
            bootstrap,
            fail_replay_update: false,
            fail_uncertainty_commit: false,
        };
        f.put("one.wav", b"original audio");
        f.put("notes.txt", b"original notes");
        f
    }
    pub(super) fn source_id(&self) -> SourceId {
        SourceId::from_string("replay-source")
    }
    pub(super) fn identity(&self) -> RootIdentity {
        source_root_identity(self.base.root.path()).unwrap()
    }
    pub(super) fn put(&self, path: &str, bytes: &[u8]) {
        fs::write(self.base.root.path().join(path), bytes).unwrap();
    }
    pub(super) fn ingress(&self, after: u64) -> ReplayIngress {
        ReplayIngress::new(
            SourceWatchSpec::new(
                self.source_id(),
                7,
                self.base.root.path().to_path_buf(),
                Some(self.identity()),
            ),
            99,
            [2; 16],
            after,
        )
        .unwrap()
    }
    pub(super) fn batch(
        &self,
        after: u64,
        events: &[(Option<u64>, &str)],
    ) -> ReplayObservationBatch {
        let ingress = self.ingress(after);
        for (id, path) in events {
            ingress.record_with_kind(
                *id,
                &self.base.root.path().join(path),
                ReplayEventCondition::Changed,
                ReplayPathKind::File,
            );
        }
        ingress.mark_history_complete();
        ingress
            .into_candidate()
            .into_observation_batch(7, WatcherGeneration::new(7))
            .unwrap()
    }
    pub(super) fn read(&self) -> SourceDatabase {
        SourceDatabase::open_for_ui_read_with_database_root(
            self.base.root.path(),
            self.base
                .guard
                .profile_root()
                .join(catalog_profile_child(&self.source_id())),
        )
        .unwrap()
    }
    pub(super) fn start(&self) -> CatalogService {
        self.start_with_gate().0
    }
    pub(super) fn start_with_gate(&self) -> (CatalogService, Arc<CatalogTestGate>) {
        let id = self.source_id();
        let admission = Arc::new(Mutex::new(AdmissionState::new()));
        admission.lock().unwrap().sources.insert(
            id.clone(),
            DesiredSource {
                lifecycle_generation: 7,
                active: true,
                attempt_token: None,
                last_curation_request_id: None,
            },
        );
        let events = Arc::new(EventHub::new());
        let (commands, rx) = mpsc::sync_channel(32);
        let worker_guard = self.base.guard.try_clone().unwrap();
        let worker_admission = admission.clone();
        let worker_events = events.clone();
        let gate = Arc::new(CatalogTestGate::new());
        let worker_gate = gate.clone();
        let root = self.base.root.path().to_path_buf();
        let bootstrap = self.bootstrap;
        let fail_replay_update = self.fail_replay_update;
        let fail_uncertainty_commit = self.fail_uncertainty_commit;
        let (ready, startup) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let db = SourceDatabase::open_for_catalog_with_profile_guard(
                root.as_path(),
                &worker_guard,
                catalog_profile_child(&id),
            )
            .unwrap();
            let fresh = db.last_authoritative_audit_metadata().unwrap().is_none();
            let publication_fence = db.source_publication_fence().unwrap();
            let db = CatalogSourceDatabase::new(db, id.clone(), 7).unwrap();
            if fresh {
                let writer = CatalogScanWriter {
                    profile_guard: &worker_guard,
                    database: &db,
                    cancel: None,
                    owner: Some(&db.writer),
                    lifecycle: None,
                };
                let _initial = audit_manifest_for_catalog_with_budget_and_progress_and_writer(
                    &db,
                    None,
                    now_epoch_seconds(),
                    &mut |_, _| {},
                    &writer,
                    None,
                )
                .unwrap();
                let previous = db.writer.current_scan_fence().unwrap().revision();
                let mut curation = db.write_batch().unwrap();
                let path = Path::new("one.wav");
                curation.set_looped(path, true).unwrap();
                curation.set_locked(path, true).unwrap();
                curation.set_tag_named(path, true).unwrap();
                curation
                    .set_user_tag(path, Some("retained curation"))
                    .unwrap();
                db.writer
                    .commit_bounded_manifest_batch(curation, previous)
                    .unwrap();
                let audit = audit_manifest_for_catalog_with_budget_and_progress_and_writer(
                    &db,
                    None,
                    now_epoch_seconds(),
                    &mut |_, _| {},
                    &writer,
                    None,
                )
                .unwrap();
                if bootstrap {
                    // Genuine producer injection is a portable harness, not live FSEvents.
                    let ingress = ReplayIngress::new(
                        SourceWatchSpec::new(
                            id.clone(),
                            7,
                            root.clone(),
                            Some(source_root_identity(&root).unwrap()),
                        ),
                        99,
                        [2; 16],
                        10,
                    )
                    .unwrap();
                    ingress.mark_history_complete();
                    let evidence = ingress
                        .into_candidate()
                        .into_bootstrap_evidence(WatcherGeneration::new(7))
                        .unwrap();
                    db.writer
                        .commit_bootstrap_replay_checkpoint(
                            &db.writer.current_scan_fence().unwrap(),
                            &audit.audit_commit,
                            &evidence,
                        )
                        .unwrap();
                }
            }
            let identity = source_root_identity(&root).unwrap();
            if fail_replay_update {
                // Fixture-only fault installed before command processing; no production alias.
                let fault = rusqlite::Connection::open(
                    worker_guard
                        .profile_root()
                        .join(catalog_profile_child(&id))
                        .join(yield_library::sample_sources::db::DB_FILE_NAME),
                )
                .unwrap();
                fault.execute_batch("CREATE TRIGGER replay_fault BEFORE UPDATE ON metadata WHEN NEW.key = 'source_watcher_checkpoint_v1' BEGIN SELECT RAISE(ABORT, 'replay fixture rollback'); END").unwrap();
            }
            if fail_uncertainty_commit {
                let fault = rusqlite::Connection::open(
                    worker_guard
                        .profile_root()
                        .join(catalog_profile_child(&id))
                        .join(yield_library::sample_sources::db::DB_FILE_NAME),
                )
                .unwrap();
                fault.execute_batch("CREATE TRIGGER marker_fault BEFORE INSERT ON metadata WHEN NEW.key = 'source_watcher_uncertainty_v1' BEGIN SELECT RAISE(ABORT, 'marker fixture rollback'); END").unwrap();
            }
            let snapshot =
                read_snapshot_from_database(&db, &worker_guard, &id, 7, &identity, || false).ok();
            let source = RegisteredSource {
                root: root.as_path().to_path_buf(),
                lifecycle_generation: 7,
                database: db,
                publication_fence,
                known_root_identity: Some(identity),
                snapshot,
                attempt_token: None,
                curation_lineage: None,
                watcher: None,
                watcher_schedule: WatcherSchedule::new(false),
                source_authority: None,
                source_authority_receipt: None,
                source_authority_pending: None,
                replay: None,
            };
            ready.send(()).unwrap();
            run_worker_with_sources(
                worker_guard,
                rx,
                worker_events,
                worker_admission,
                Some(worker_gate),
                None,
                WatcherMode::Detached,
                None,
                HashMap::from([(id, source)]),
            )
        });
        startup.recv_timeout(Duration::from_secs(5)).unwrap();
        (
            CatalogService {
                commands,
                events,
                admission,
                worker: Mutex::new(Some(worker)),
            },
            gate,
        )
    }
    pub(super) fn receive(request: &CatalogReplayRequest) -> CatalogReplayOutcome {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(result) = request.try_recv().unwrap() {
                return result;
            }
            assert!(
                Instant::now() < deadline,
                "actual replay worker did not reply"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }
}
