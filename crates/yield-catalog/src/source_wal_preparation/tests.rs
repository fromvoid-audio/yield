//! Actual catalog-owner tests, all fixture helpers compiled only in the test module.
#[cfg(test)]
mod integration {
    //! Actual catalog-worker/profile SQLite tests; no native capacity qualification is fabricated.
    use super::super::*;
    use rusqlite::{Connection, ErrorCode, OpenFlags};
    use yield_library::operation_journal::{AdmissionClaimId, ParticipantId};
    use yield_library::sample_sources::db::DB_FILE_NAME;

    struct Fixture {
        base: crate::tests::Fixture,
        second: tempfile::TempDir,
        long_id: bool,
        mixed: bool,
        local_prepared_authorities: bool,
    }
    impl Fixture {
        fn new() -> Self {
            let f = Self {
                base: crate::tests::Fixture::new("catalog-wal-custody"),
                second: tempfile::tempdir().unwrap(),
                long_id: false,
                mixed: false,
                local_prepared_authorities: false,
            };
            for root in [f.base.root.path(), f.second.path()] {
                let mut wav = hound::WavWriter::create(
                    root.join("one.wav"),
                    hound::WavSpec {
                        channels: 1,
                        sample_rate: 8000,
                        bits_per_sample: 16,
                        sample_format: hound::SampleFormat::Int,
                    },
                )
                .unwrap();
                for _ in 0..8 {
                    wav.write_sample(1i16).unwrap();
                }
                wav.finalize().unwrap();
            }
            f
        }
        fn start(
            &self,
            deny: Option<Arc<AtomicBool>>,
        ) -> (CatalogService, Vec<SourceRevision>, Arc<CatalogTestGate>) {
            self.start_with_faults(deny, None)
        }
        fn start_with_faults(
            &self,
            deny: Option<Arc<AtomicBool>>,
            restore_fault: Option<Arc<AtomicBool>>,
        ) -> (CatalogService, Vec<SourceRevision>, Arc<CatalogTestGate>) {
            self.start_inner(deny, restore_fault, None)
        }
        fn start_inner(
            &self,
            deny: Option<Arc<AtomicBool>>,
            restore_fault: Option<Arc<AtomicBool>>,
            journal: Option<OperationJournalClient>,
        ) -> (CatalogService, Vec<SourceRevision>, Arc<CatalogTestGate>) {
            let (service, revisions, _, gate) =
                self.start_mode(deny, restore_fault, journal, false);
            (service, revisions, gate)
        }
        fn start_lease(
            &self,
            deny: Option<Arc<AtomicBool>>,
            restore_fault: Option<Arc<AtomicBool>>,
        ) -> (
            CatalogService,
            Vec<yield_library::sample_sources::db::write::SourceLeaseMaintenanceAllocation>,
            Arc<CatalogTestGate>,
        ) {
            let (service, _, inputs, gate) = self.start_mode(deny, restore_fault, None, true);
            (service, inputs, gate)
        }
        fn start_mode(
            &self,
            deny: Option<Arc<AtomicBool>>,
            restore_fault: Option<Arc<AtomicBool>>,
            journal: Option<OperationJournalClient>,
            preaudit: bool,
        ) -> (
            CatalogService,
            Vec<SourceRevision>,
            Vec<yield_library::sample_sources::db::write::SourceLeaseMaintenanceAllocation>,
            Arc<CatalogTestGate>,
        ) {
            let admission = Arc::new(Mutex::new(AdmissionState::new()));
            let events = Arc::new(EventHub::new());
            let (commands, rx) = mpsc::sync_channel(COMMAND_CAPACITY);
            let (ready, startup) = mpsc::sync_channel(1);
            let guard = self.base.guard.try_clone().unwrap();
            let roots = vec![
                self.base.root.path().to_path_buf(),
                self.second.path().to_path_buf(),
            ];
            let long_id = self.long_id;
            let mixed = self.mixed;
            let local_prepared_authorities = self.local_prepared_authorities || preaudit;
            let worker_admission = admission.clone();
            let worker_events = events.clone();
            let gate = Arc::new(CatalogTestGate::new());
            let worker_gate = gate.clone();
            let worker = thread::spawn(move || {
                let mut sources = HashMap::new();
                let mut revisions = Vec::new();
                let mut lease_inputs = Vec::new();
                for (index, root) in roots.into_iter().enumerate() {
                    let id = SourceId::from_string(if long_id {
                        format!("wal-{index}-{}", "x".repeat(200))
                    } else {
                        format!("wal-{index}")
                    });
                    let database = SourceDatabase::open_for_catalog_with_profile_guard(
                        &root,
                        &guard,
                        catalog_profile_child(&id),
                    )
                    .unwrap();
                    let publication_fence = database.source_publication_fence().unwrap();
                    let mut db = CatalogSourceDatabase::new(database, id.clone(), 1).unwrap();
                    if !preaudit || (mixed && index == 1) {
                        let scan_writer = CatalogScanWriter {
                            profile_guard: &guard,
                            database: &db,
                            cancel: None,
                            owner: Some(&db.writer),
                            lifecycle: None,
                        };
                        audit_manifest_for_catalog_with_budget_and_progress_and_writer(
                            &db,
                            None,
                            now_epoch_seconds(),
                            &mut |_, _| {},
                            &scan_writer,
                            None,
                        )
                        .unwrap();
                    }
                    let (source_authority, source_authority_receipt) =
                        if let Some(journal) = journal.as_ref() {
                            let authority = db.prepare_source_authority().unwrap();
                            let mut request = journal
                                .register_source_authority(authority.journal_request().unwrap())
                                .unwrap();
                            let receipt = request
                                .recv_timeout(SOURCE_AUTHORITY_HANDOFF_TIMEOUT)
                                .unwrap()
                                .unwrap();
                            (Some(authority), Some(receipt))
                        } else if local_prepared_authorities {
                            // Genuine local Prepared guards; never pretend journal/reserve registration.
                            (Some(db.prepare_source_authority().unwrap()), None)
                        } else {
                            (None, None)
                        };
                    if index == 0 {
                        if let Some(fault) = &restore_fault {
                            db.writer
                                .set_wal_timeout_restore_fault_for_tests(fault.clone());
                        }
                    }
                    if index == 0 {
                        if let Some(deny) = &deny {
                            db.writer.set_wal_rollback_denial_for_tests(deny.clone());
                        }
                    }
                    if preaudit {
                        if !(mixed && index == 1) {
                            assert!(db.writer.current_revision().is_err());
                        }
                        lease_inputs.push(
                            db.writer
                                .capture_lease_maintenance_allocation(deadline())
                                .unwrap(),
                        );
                    }
                    if let Ok(revision) = db.writer.current_revision() {
                        revisions.push(revision);
                    }
                    let identity = source_root_identity(&root).unwrap();
                    let snapshot = if preaudit && !(mixed && index == 1) {
                        None
                    } else {
                        Some(
                            read_snapshot_from_database(&db, &guard, &id, 1, &identity, || false)
                                .unwrap(),
                        )
                    };
                    worker_admission.lock().unwrap().sources.insert(
                        id.clone(),
                        DesiredSource {
                            lifecycle_generation: 1,
                            active: true,
                            attempt_token: None,
                            last_curation_request_id: None,
                        },
                    );
                    sources.insert(
                        id,
                        RegisteredSource {
                            collection_history_token: None,
                            replay: None,
                            root: root.clone(),
                            lifecycle_generation: 1,
                            database: db,
                            publication_fence,
                            known_root_identity: Some(source_root_identity(&root).unwrap()),
                            snapshot,
                            attempt_token: None,
                            curation_lineage: None,
                            watcher: None,
                            watcher_schedule: WatcherSchedule::new(false),
                            source_authority,
                            source_authority_receipt,
                            source_authority_pending: None,
                        },
                    );
                }
                ready.send((revisions, lease_inputs)).unwrap();
                run_worker_with_sources(
                    guard,
                    rx,
                    worker_events,
                    worker_admission,
                    Some(worker_gate),
                    journal,
                    WatcherMode::Detached,
                    None,
                    sources,
                );
            });
            let (revisions, lease_inputs) = startup.recv_timeout(Duration::from_secs(5)).unwrap();
            (
                CatalogService {
                    commands,
                    events,
                    admission,
                    worker: Mutex::new(crate::shutdown::WorkerJoinState::new(Some(worker))),
                },
                revisions,
                lease_inputs,
                gate,
            )
        }
        fn external(&self, revision: &SourceRevision) -> Connection {
            self.external_id(revision.source_id())
        }
        fn external_id(&self, id: &SourceId) -> Connection {
            let path = self
                .base
                .guard
                .profile_root()
                .join(catalog_profile_child(id))
                .join(DB_FILE_NAME);
            let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
            db.busy_timeout(Duration::ZERO).unwrap();
            db
        }
    }
    fn correlation() -> CatalogSourceWalCorrelation {
        CatalogSourceWalCorrelation::new(
            [9; 32],
            ParticipantId::new(),
            AdmissionClaimId::new(),
            AdmissionClaimId::new(),
        )
        .unwrap()
    }
    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(2)
    }
    fn wait(
        request: &CatalogSourceWalRequest,
        predicate: impl Fn(&CatalogSourceWalStatus) -> bool,
    ) -> CatalogSourceWalStatus {
        let end = Instant::now() + Duration::from_secs(4);
        loop {
            let status = request.status();
            if predicate(&status) {
                return status;
            }
            assert!(
                Instant::now() < end,
                "catalog preparation outcome: {status:?}"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }
    fn prepared(request: &mut CatalogSourceWalRequest) -> CatalogSourceWalPreparation {
        wait(request, |s| {
            matches!(s, CatalogSourceWalStatus::Prepared(_))
        });
        request.try_prepared().unwrap()
    }
    fn busy(db: &Connection) {
        let error = db.execute_batch("BEGIN IMMEDIATE").unwrap_err();
        assert_eq!(error.sqlite_error_code(), Some(ErrorCode::DatabaseBusy));
    }
    fn read(revision: &SourceRevision) -> CatalogCommand {
        CatalogCommand::ReadSnapshot {
            source_id: revision.source_id().clone(),
            expected_revision: Some(revision.revision()),
        }
    }
    fn credits(service: &CatalogService) -> (usize, usize) {
        let state = service.admission.lock().unwrap();
        state.source_wal.context_usage_for_tests()
    }
    fn lease(db: &Connection) -> (String, i64, i64, i64, String) {
        db.query_row("SELECT owner_identity,lease_epoch,acquired_at,expires_at,state FROM source_lease WHERE singleton=1", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap()
    }
    fn metadata(db: &Connection) -> Vec<(String, String)> {
        db.prepare("SELECT key,value FROM metadata ORDER BY key")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn simultaneous_actual_sources_lock_without_renewal_or_rows_and_abort_restores_exact_revision()
    {
        let f = Fixture::new();
        let (service, revisions, _) = f.start(None);
        let a = f.external(&revisions[0]);
        let b = f.external(&revisions[1]);
        let before = [(lease(&a), metadata(&a)), (lease(&b), metadata(&b))];
        let mut first = service
            .prepare_source_wal(revisions[0].clone(), correlation(), deadline())
            .unwrap();
        let mut second = service
            .prepare_source_wal(revisions[1].clone(), correlation(), deadline())
            .unwrap();
        let held_a = prepared(&mut first);
        let held_b = prepared(&mut second);
        busy(&a);
        busy(&b);
        assert_eq!(
            held_a.acknowledge_settlement(),
            Err(CatalogSourceWalRejection::SettlementUnsupported)
        );
        assert_eq!(
            [(lease(&a), metadata(&a)), (lease(&b), metadata(&b))],
            before
        );
        assert_eq!(credits(&service).0, 2);
        let duplicate = service
            .prepare_source_wal(revisions[0].clone(), correlation(), deadline())
            .unwrap();
        wait(&duplicate, |s| {
            *s == CatalogSourceWalStatus::Rejected(CatalogSourceWalRejection::Busy)
        });
        drop(held_a);
        drop(held_b);
        wait(&first, |s| *s == CatalogSourceWalStatus::Aborted);
        wait(&second, |s| *s == CatalogSourceWalStatus::Aborted);
        assert_eq!(credits(&service), (0, 0));
        a.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        b.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        let mut again = service
            .prepare_source_wal(revisions[0].clone(), correlation(), deadline())
            .unwrap();
        let held = prepared(&mut again);
        assert!(
            matches!(held.status(), CatalogSourceWalStatus::Prepared(ref o) if o.revision() == &revisions[0])
        );
        drop(held);
        wait(&again, |s| *s == CatalogSourceWalStatus::Aborted);
        service.shutdown();
    }

    #[test]
    fn source_deferral_keeps_total_normal_queue_32_and_other_source_dispatch_correlated() {
        let f = Fixture::new();
        let (service, revisions, _) = f.start(None);
        let a = f.external(&revisions[0]);
        let mut request = service
            .prepare_source_wal(revisions[0].clone(), correlation(), deadline())
            .unwrap();
        let held = prepared(&mut request);
        service.submit(read(&revisions[0])).unwrap();
        service
            .submit(CatalogCommand::RequestTagPage {
                source_id: revisions[1].source_id().clone(),
                lifecycle_generation: 1,
                expected_revision: revisions[1].revision(),
                request_id: 22,
                query: CatalogTagPageQuery::SelectedPaths {
                    relative_paths: vec![PathBuf::from("one.wav")],
                },
                after_cursor: None,
                page_limit: 1,
            })
            .unwrap();
        let end = Instant::now() + Duration::from_secs(1);
        loop {
            if let Ok(CatalogEvent::TagPageReady { source_id, .. }) = service.try_recv_event() {
                assert_eq!(source_id, *revisions[1].source_id());
                break;
            }
            assert!(Instant::now() < end, "unrelated source did not progress");
            thread::sleep(Duration::from_millis(2));
        }
        busy(&a);
        let metrics = service.try_command_metrics().unwrap();
        assert_eq!((metrics.dispatched, metrics.pending_dispatch), (1, 1));
        for _ in 1..COMMAND_CAPACITY {
            service.submit(read(&revisions[0])).unwrap();
        }
        assert_eq!(
            service.submit(read(&revisions[0])),
            Err(CatalogSubmitError::Full)
        );
        assert_eq!(service.admission.lock().unwrap().queued_commands, 32);
        assert_eq!(service.try_command_metrics().unwrap().pending_dispatch, 32);
        drop(held);
        wait(&request, |s| *s == CatalogSourceWalStatus::Aborted);
        service.shutdown();
        let metrics = service.try_command_metrics().unwrap();
        assert_eq!(
            (
                metrics.dispatched,
                metrics.pending_dispatch,
                metrics.untracked_dispatches
            ),
            (33, 0, 0)
        );
        assert_eq!(service.admission.lock().unwrap().queued_commands, 0);
        assert_eq!(credits(&service), (0, 0));
    }

    #[test]
    fn pending_drop_and_deadline_include_queue_wait_without_sql_or_immediate_credit_refund() {
        let f = Fixture::new();
        let (service, revisions, gate) = f.start(None);
        let pause = gate.pause_next();
        let db = f.external(&revisions[0]);
        let before = (lease(&db), metadata(&db));
        let request = service
            .prepare_source_wal(revisions[0].clone(), correlation(), deadline())
            .unwrap();
        let ticket = request.outcome_probe_for_tests();
        drop(request);
        assert_eq!(credits(&service).0, 1);
        let expired = service
            .prepare_source_wal(
                revisions[1].clone(),
                correlation(),
                Instant::now() + Duration::from_millis(30),
            )
            .unwrap();
        thread::sleep(Duration::from_millis(50));
        db.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        assert_eq!(credits(&service).0, 2);
        drop(pause);
        wait(&expired, |s| {
            *s == CatalogSourceWalStatus::Rejected(CatalogSourceWalRejection::Deadline)
        });
        let end = Instant::now() + Duration::from_secs(1);
        while ticket.status()
            != CatalogSourceWalStatus::Rejected(CatalogSourceWalRejection::Cancelled)
        {
            assert!(Instant::now() < end);
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(credits(&service), (0, 0));
        assert_eq!((lease(&db), metadata(&db)), before);
        service.shutdown();
    }

    #[test]
    fn real_rollback_denial_keeps_original_owner_private_blocks_replacement_and_retries() {
        let deny = Arc::new(AtomicBool::new(false));
        let f = Fixture::new();
        let (service, revisions, _) = f.start(Some(deny.clone()));
        let db = f.external(&revisions[0]);
        let before = metadata(&db);
        let mut request = service
            .prepare_source_wal(revisions[0].clone(), correlation(), deadline())
            .unwrap();
        let held = prepared(&mut request);
        deny.store(true, Ordering::Release);
        held.abort();
        wait(&request, |s| {
            matches!(s, CatalogSourceWalStatus::Unavailable(_))
        });
        busy(&db);
        assert_eq!(credits(&service).0, 1);
        service
            .submit(CatalogCommand::Register {
                source_id: revisions[0].source_id().clone(),
                canonical_root: f.base.root.path().to_path_buf(),
                lifecycle_generation: 2,
            })
            .unwrap();
        let end = Instant::now() + Duration::from_secs(1);
        loop {
            if let Ok(CatalogEvent::Failed {
                operation: CatalogOperation::Register,
                error: CatalogError::SourceWriterConflict(_),
                ..
            }) = service.try_recv_event()
            {
                break;
            }
            assert!(Instant::now() < end);
            thread::sleep(Duration::from_millis(2));
        }
        busy(&db);
        assert_eq!(metadata(&db), before);
        assert!(!matches!(
            held.status(),
            CatalogSourceWalStatus::Aborted | CatalogSourceWalStatus::Retired
        ));
        deny.store(false, Ordering::Release);
        wait(&request, |s| {
            matches!(
                s,
                CatalogSourceWalStatus::Rejected(CatalogSourceWalRejection::Scope)
                    | CatalogSourceWalStatus::Aborted
            )
        });
        db.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        assert_eq!(credits(&service), (0, 0));
        service.shutdown();
    }

    #[test]
    fn aggregate_pending_context_is_bounded_and_worker_close_refunds_unacquired_controls() {
        let f = Fixture::new();
        let (service, revisions, gate) = f.start(None);
        let pause = gate.pause_next();
        let mut requests = Vec::new();
        loop {
            match service.prepare_source_wal(revisions[0].clone(), correlation(), deadline()) {
                Ok(request) => requests.push(request),
                Err(CatalogSourceWalRejection::Full) => break,
                Err(e) => panic!("{e}"),
            }
        }
        let (slots, bytes) = credits(&service);
        assert_eq!(slots, requests.len());
        assert!(slots <= 64);
        assert!(bytes <= 64 * 1024);
        assert!(slots > 1); // Real identities/correlations, not placeholder strings.
        {
            let state = service.admission.lock().unwrap();
            state.source_wal.close();
        }
        assert_eq!(credits(&service), (0, 0));
        assert!(
            requests.iter().all(|r| r.status()
                == CatalogSourceWalStatus::Rejected(CatalogSourceWalRejection::Closed))
        );
        drop(pause);
        service.shutdown();
    }

    #[test]
    fn active_deadline_and_lifecycle_invalidation_are_owner_driven_empty_rollback() {
        let f = Fixture::new();
        let (service, revisions, _) = f.start(None);
        let db = f.external(&revisions[0]);
        let mut request = service
            .prepare_source_wal(
                revisions[0].clone(),
                correlation(),
                Instant::now() + Duration::from_millis(150),
            )
            .unwrap();
        let held = prepared(&mut request);
        busy(&db);
        wait(&request, |s| {
            *s == CatalogSourceWalStatus::Rejected(CatalogSourceWalRejection::Deadline)
        });
        db.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        assert_eq!(credits(&service), (0, 0));
        drop(held);
        let mut request = service
            .prepare_source_wal(revisions[0].clone(), correlation(), deadline())
            .unwrap();
        let held = prepared(&mut request);
        service
            .admission
            .lock()
            .unwrap()
            .sources
            .get_mut(revisions[0].source_id())
            .unwrap()
            .active = false;
        wait(&request, |s| {
            *s == CatalogSourceWalStatus::Rejected(CatalogSourceWalRejection::Scope)
        });
        db.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        assert_eq!(credits(&service), (0, 0));
        drop(held);
        service.shutdown();
    }

    #[test]
    fn queued_curation_replay_and_read_keep_credits_and_execute_only_after_owner_abort() {
        use yield_library::sample_sources::reconciliation::WatcherGeneration;
        use yield_source_watcher::replay::{ReplayEventCondition, ReplayIngress, ReplayPathKind};
        let f = Fixture::new();
        let (service, revisions, _) = f.start(None);
        let revision = &revisions[0];
        let db = f.external(revision);
        let before = metadata(&db);
        let mut request = service
            .prepare_source_wal(revision.clone(), correlation(), deadline())
            .unwrap();
        let held = prepared(&mut request);
        let ingress = ReplayIngress::new(
            SourceWatchSpec::new(
                revision.source_id().clone(),
                1,
                f.base.root.path().to_path_buf(),
                Some(revision.root_identity().clone()),
            ),
            4,
            [3; 16],
            0,
        )
        .unwrap();
        ingress.record_with_kind(
            Some(1),
            &f.base.root.path().join("one.wav"),
            ReplayEventCondition::Changed,
            ReplayPathKind::File,
        );
        ingress.mark_history_complete();
        service
            .submit(CatalogCommand::ApplyCuration {
                source_id: revision.source_id().clone(),
                lifecycle_generation: 1,
                expected_revision: revision.revision(),
                root_identity: revision.root_identity().clone(),
                request_id: 100,
                updates: vec![SourceCurationUpdate::SetLocked {
                    path: RootRelativePath::try_from_path(PathBuf::from("one.wav")).unwrap(),
                    locked: true,
                }],
            })
            .unwrap();
        let replay = service
            .submit_replay(
                ingress
                    .into_candidate()
                    .into_observation_batch(1, WatcherGeneration::new(1))
                    .unwrap(),
                1,
                revision.revision(),
            )
            .unwrap();
        service.submit(read(revision)).unwrap();
        thread::sleep(Duration::from_millis(70)); // Covers several real worker idle/control polls.
        assert!(replay.try_recv().unwrap().is_none());
        busy(&db);
        assert_eq!(metadata(&db), before);
        {
            let state = service.admission.lock().unwrap();
            assert_eq!(state.queued_commands, 3);
            assert!(state.curation_payload_bytes > 0);
            assert_eq!(state.curation_update_count, 0);
        }
        assert_eq!(service.try_command_metrics().unwrap().dispatched, 0);
        drop(held);
        wait(&request, |s| *s == CatalogSourceWalStatus::Aborted);
        let end = Instant::now() + Duration::from_secs(2);
        loop {
            if replay.try_recv().unwrap().is_some() {
                break;
            }
            assert!(Instant::now() < end);
            thread::sleep(Duration::from_millis(2));
        }
        let mut committed = false;
        while Instant::now() < end {
            if let Ok(CatalogEvent::CurationCommitted {
                request_id: 100, ..
            }) = service.try_recv_event()
            {
                committed = true;
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(
            committed,
            "accepted curation did not return its genuine terminal receipt"
        );
        service.shutdown();
        let state = service.admission.lock().unwrap();
        assert_eq!(
            (
                state.queued_commands,
                state.curation_payload_bytes,
                state.curation_update_count
            ),
            (0, 0, 0)
        );
        assert_eq!(credits_after_unlock(&state), (0, 0));
    }
    fn credits_after_unlock(state: &AdmissionState) -> (usize, usize) {
        state.source_wal.context_usage_for_tests()
    }

    #[test]
    fn failed_timeout_restoration_keeps_real_writer_private_until_policy_retry_succeeds() {
        let f = Fixture::new();
        let fault = Arc::new(AtomicBool::new(true));
        let (service, revisions, _) = f.start_with_faults(None, Some(fault.clone()));
        let db = f.external(&revisions[0]);
        let before = metadata(&db);
        let request = service
            .prepare_source_wal(revisions[0].clone(), correlation(), deadline())
            .unwrap();
        wait(&request, |s| {
            matches!(s, CatalogSourceWalStatus::Unavailable(_))
        });
        assert_eq!(credits(&service).0, 1);
        assert_eq!(metadata(&db), before);
        // Rollback can be confirmed, but failed restoration still prevents a callable owner.
        db.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        service.submit(read(&revisions[0])).unwrap();
        thread::sleep(Duration::from_millis(50));
        assert_eq!(service.try_command_metrics().unwrap().dispatched, 0);
        fault.store(false, Ordering::Release);
        wait(&request, |s| {
            *s == CatalogSourceWalStatus::Rejected(CatalogSourceWalRejection::WalUnavailable)
        });
        assert_eq!(credits(&service), (0, 0));
        service.shutdown();
    }

    #[test]
    fn stale_revision_missing_registration_busy_and_near_expiry_never_renew_or_write() {
        let f = Fixture::new();
        let (service, revisions, _) = f.start(None);
        let db = f.external(&revisions[0]);
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        let before = metadata(&db);
        let request = service
            .prepare_source_wal(revisions[0].clone(), correlation(), deadline())
            .unwrap();
        wait(&request, |s| {
            *s == CatalogSourceWalStatus::Rejected(CatalogSourceWalRejection::WalUnavailable)
        });
        assert_eq!(credits(&service), (0, 0));
        db.execute_batch("ROLLBACK").unwrap();
        assert_eq!(metadata(&db), before);
        db.execute(
            "UPDATE source_lease SET expires_at=? WHERE singleton=1",
            [now_epoch_seconds() + 2],
        )
        .unwrap();
        let near = lease(&db);
        let request = service
            .prepare_source_wal(revisions[0].clone(), correlation(), deadline())
            .unwrap();
        wait(&request, |s| {
            *s == CatalogSourceWalStatus::Rejected(CatalogSourceWalRejection::LeaseRenewalRequired)
        });
        assert_eq!(lease(&db), near);
        assert_eq!(metadata(&db), before);
        assert_eq!(credits(&service), (0, 0));
        db.execute(
            "UPDATE source_lease SET expires_at=? WHERE singleton=1",
            [now_epoch_seconds() + 120],
        )
        .unwrap();
        db.execute(
            "UPDATE metadata SET value=? WHERE key='revision'",
            [(revisions[0].revision() + 1).to_string()],
        )
        .unwrap();
        let stale_before = metadata(&db);
        let stale = service
            .prepare_source_wal(revisions[0].clone(), correlation(), deadline())
            .unwrap();
        wait(&stale, |s| {
            *s == CatalogSourceWalStatus::Rejected(CatalogSourceWalRejection::Scope)
        });
        assert_eq!(metadata(&db), stale_before);
        assert_eq!(credits(&service), (0, 0));
        service
            .admission
            .lock()
            .unwrap()
            .sources
            .get_mut(revisions[1].source_id())
            .unwrap()
            .lifecycle_generation = 2;
        assert!(matches!(
            service.prepare_source_wal(revisions[1].clone(), correlation(), deadline()),
            Err(CatalogSourceWalRejection::Scope)
        ));
        service
            .admission
            .lock()
            .unwrap()
            .sources
            .remove(revisions[1].source_id());
        assert!(matches!(
            service.prepare_source_wal(revisions[1].clone(), correlation(), deadline()),
            Err(CatalogSourceWalRejection::Scope)
        ));
        assert!(matches!(
            service.prepare_source_wal(revisions[0].clone(), correlation(), Instant::now()),
            Err(CatalogSourceWalRejection::Deadline)
        ));
        assert!(matches!(
            service.prepare_source_wal(
                revisions[0].clone(),
                correlation(),
                Instant::now() + Duration::from_secs(3)
            ),
            Err(CatalogSourceWalRejection::Deadline)
        ));
        service.shutdown();
    }

    #[test]
    fn retained_root_loss_drives_owner_abort_and_never_invents_retirement() {
        let f = Fixture::new();
        let (service, revisions, _) = f.start(None);
        let db = f.external(&revisions[0]);
        let before = metadata(&db);
        let mut request = service
            .prepare_source_wal(revisions[0].clone(), correlation(), deadline())
            .unwrap();
        let held = prepared(&mut request);
        let moved = f.base.root.path().with_extension("parked-old-root");
        fs::rename(f.base.root.path(), &moved).unwrap();
        let status = wait(&request, |s| {
            matches!(
                s,
                CatalogSourceWalStatus::Rejected(CatalogSourceWalRejection::Scope)
            )
        });
        assert_ne!(status, CatalogSourceWalStatus::Retired);
        db.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        fs::rename(&moved, f.base.root.path()).unwrap();
        assert_eq!(metadata(&db), before);
        assert_eq!(credits(&service), (0, 0));
        drop(held);
        service.shutdown();
    }

    #[test]
    fn shutdown_aborts_delivered_owner_and_runs_accepted_durable_curation_before_connection_close()
    {
        let f = Fixture::new();
        let (service, revisions, _) = f.start(None);
        let revision = &revisions[0];
        let db = f.external(revision);
        let mut request = service
            .prepare_source_wal(revision.clone(), correlation(), deadline())
            .unwrap();
        let held = prepared(&mut request);
        service
            .submit(CatalogCommand::ApplyCuration {
                source_id: revision.source_id().clone(),
                lifecycle_generation: 1,
                expected_revision: revision.revision(),
                root_identity: revision.root_identity().clone(),
                request_id: 101,
                updates: vec![SourceCurationUpdate::SetLocked {
                    path: RootRelativePath::try_from_path(PathBuf::from("one.wav")).unwrap(),
                    locked: true,
                }],
            })
            .unwrap();
        service.shutdown();
        assert!(matches!(
            held.status(),
            CatalogSourceWalStatus::Rejected(CatalogSourceWalRejection::Scope)
                | CatalogSourceWalStatus::Aborted
        ));
        assert_eq!(
            db.query_row("SELECT value FROM metadata WHERE key='revision'", [], |r| r
                .get::<_, String>(0))
                .unwrap()
                .parse::<u64>()
                .unwrap(),
            revision.revision() + 1
        );
        assert_eq!(credits(&service), (0, 0));
        assert!(
            service.admission.lock().unwrap().curation_payload_bytes > 0,
            "delivered terminal payload retains its existing event credit"
        );
        let mut committed = false;
        let mut closed = false;
        while let Ok(event) = service.try_recv_event() {
            match event {
                CatalogEvent::CurationCommitted {
                    request_id: 101, ..
                } => {
                    assert!(!closed);
                    committed = true;
                }
                CatalogEvent::Closed => {
                    assert!(committed);
                    closed = true;
                }
                _ => {}
            }
        }
        assert!(committed && closed);
        let state = service.admission.lock().unwrap();
        assert_eq!(
            (
                state.queued_commands,
                state.curation_payload_bytes,
                state.curation_update_count,
                state.event_budget
            ),
            (0, 0, 0, 0)
        );
    }

    #[test]
    fn unknown_retained_wal_vfs_rejects_actual_worker_without_effect_and_returns_credits() {
        let f = Fixture::new();
        let (service, revisions, _) = f.start(None);
        let db = f.external(&revisions[0]);
        let before = (lease(&db), metadata(&db));
        service
            .admission
            .lock()
            .unwrap()
            .source_wal
            .set_unknown_wal_for_tests(true);
        let request = service
            .prepare_source_wal(revisions[0].clone(), correlation(), deadline())
            .unwrap();
        wait(&request, |s| {
            *s == CatalogSourceWalStatus::Rejected(CatalogSourceWalRejection::WalUnavailable)
        });
        assert_eq!((lease(&db), metadata(&db)), before);
        assert_eq!(credits(&service), (0, 0));
        db.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        service
            .admission
            .lock()
            .unwrap()
            .source_wal
            .set_unknown_wal_for_tests(false);
        let mut retry = service
            .prepare_source_wal(revisions[0].clone(), correlation(), deadline())
            .unwrap();
        let held = prepared(&mut retry);
        drop(held);
        wait(&retry, |s| *s == CatalogSourceWalStatus::Aborted);
        service.shutdown();
    }

    #[test]
    fn real_reader_pressure_rejects_actual_catalog_reservation_without_checkpoint_or_effect() {
        let f = Fixture::new();
        let (service, revisions, _) = f.start(None);
        let db = f.external(&revisions[0]);
        db.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        let path = f
            .base
            .guard
            .profile_root()
            .join(catalog_profile_child(revisions[0].source_id()))
            .join(DB_FILE_NAME);
        let reader = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        reader
            .execute_batch("BEGIN; SELECT count(*) FROM metadata")
            .unwrap();
        db.execute(
            "INSERT INTO metadata(key,value) VALUES ('reservation-pressure',zeroblob(?))",
            [yield_library::WAL_HARD_WATERMARK_BYTES as i64],
        )
        .unwrap();
        // Avoid copying the large fixture BLOB; observe its exact retained size and unchanged lease.
        let before = lease(&db);
        let revision = revisions[0].revision();
        let request = service
            .prepare_source_wal(revisions[0].clone(), correlation(), deadline())
            .unwrap();
        wait(&request, |s| {
            *s == CatalogSourceWalStatus::Rejected(CatalogSourceWalRejection::WalUnavailable)
        });
        assert_eq!(credits(&service), (0, 0));
        assert_eq!(lease(&db), before);
        assert_eq!(
            db.query_row("SELECT value FROM metadata WHERE key='revision'", [], |r| r
                .get::<_, String>(0))
                .unwrap()
                .parse::<u64>()
                .unwrap(),
            revision
        );
        assert_eq!(
            reader
                .query_row(
                    "SELECT count(*) FROM metadata WHERE key='reservation-pressure'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        reader.execute_batch("ROLLBACK").unwrap();
        drop(reader);
        db.execute("DELETE FROM metadata WHERE key='reservation-pressure'", [])
            .unwrap();
        db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
        let mut retry = service
            .prepare_source_wal(revisions[0].clone(), correlation(), deadline())
            .unwrap();
        let held = prepared(&mut retry);
        drop(held);
        wait(&retry, |s| *s == CatalogSourceWalStatus::Aborted);
        service.shutdown();
    }

    #[test]
    fn lost_delivery_holder_and_sticky_shutdown_never_claim_reuse_or_retirement_without_ack() {
        let f = Fixture::new();
        let deny = Arc::new(AtomicBool::new(false));
        let (service, revisions, gate) = f.start(Some(deny.clone()));
        let db = f.external(&revisions[0]);
        let request = service
            .prepare_source_wal(revisions[0].clone(), correlation(), deadline())
            .unwrap();
        wait(&request, |s| {
            matches!(s, CatalogSourceWalStatus::Prepared(_))
        });
        let witness = request.outcome_probe_for_tests();
        let pause = gate.pause_next();
        drop(request);
        busy(&db);
        assert_eq!(credits(&service).0, 1);
        drop(pause);
        let end = Instant::now() + Duration::from_secs(1);
        while witness.status() != CatalogSourceWalStatus::Aborted {
            assert!(Instant::now() < end);
            thread::sleep(Duration::from_millis(2));
        }
        let mut request = service
            .prepare_source_wal(revisions[0].clone(), correlation(), deadline())
            .unwrap();
        let held = prepared(&mut request);
        deny.store(true, Ordering::Release);
        held.abort();
        wait(&request, |s| {
            matches!(s, CatalogSourceWalStatus::Unavailable(_))
        });
        service
            .submit(CatalogCommand::ApplyCuration {
                source_id: revisions[0].source_id().clone(),
                lifecycle_generation: 1,
                expected_revision: revisions[0].revision(),
                root_identity: revisions[0].root_identity().clone(),
                request_id: 103,
                updates: vec![SourceCurationUpdate::SetLocked {
                    path: RootRelativePath::try_from_path(PathBuf::from("one.wav")).unwrap(),
                    locked: true,
                }],
            })
            .unwrap();
        service.shutdown();
        let mut honest_failure = false;
        while let Ok(event) = service.try_recv_event() {
            match event {
                CatalogEvent::CurationFailed {
                    request_id: 103,
                    error: CatalogError::SourceWriterConflict(_),
                    ..
                } => honest_failure = true,
                CatalogEvent::CurationCommitted {
                    request_id: 103, ..
                } => panic!("unavailable writer cannot invent a commit"),
                _ => {}
            }
        }
        assert!(honest_failure);
        {
            let state = service.admission.lock().unwrap();
            assert_eq!(
                (
                    state.queued_commands,
                    state.curation_payload_bytes,
                    state.event_budget
                ),
                (0, 0, 0)
            );
        }
        assert_eq!(
            held.status(),
            CatalogSourceWalStatus::Unavailable(CatalogSourceWalRejection::OwnerLost)
        );
        assert_eq!(
            credits(&service).0,
            1,
            "unconfirmed retirement cannot refund a slot using an invented ack"
        );
        db.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap(); // Actual worker drop closed connection; this is not a sealed source ack.
    }

    #[test]
    fn long_other_source_work_aborts_pre_intent_custody_before_dispatching_broad_snapshot() {
        let f = Fixture::new();
        let (service, revisions, _) = f.start(None);
        let db = f.external(&revisions[0]);
        let mut request = service
            .prepare_source_wal(revisions[0].clone(), correlation(), deadline())
            .unwrap();
        let held = prepared(&mut request);
        service.submit(read(&revisions[1])).unwrap();
        wait(&request, |s| *s == CatalogSourceWalStatus::Aborted);
        db.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        assert_eq!(credits(&service), (0, 0));
        drop(held);
        service.shutdown();
    }

    #[test]
    #[ignore = "requires qualified native protected-reserve source authority; OPT-2026"]
    fn native_failed_reservation_uses_original_drain_close_ack_and_finalization() {
        use yield_library::operation_journal::{
            OperationJournalOwner, OperationJournalOwnerStatus, OperationJournalStore,
        };
        let f = Fixture::new();
        let registry = tempfile::tempdir().unwrap();
        let registry_path = registry.path().join("registry");
        fs::create_dir(&registry_path).unwrap();
        let mut journal = OperationJournalOwner::start_with_capacity_fixture_for_test(
            OperationJournalStore::open(&f.base.guard).unwrap(),
            registry_path,
        )
        .unwrap();
        let end = Instant::now() + Duration::from_secs(5);
        while journal.status() == OperationJournalOwnerStatus::Initializing {
            assert!(Instant::now() < end);
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(
            journal.status(),
            OperationJournalOwnerStatus::Available,
            "native bootstrap must actually qualify; no model substitute"
        );
        let deny = Arc::new(AtomicBool::new(false));
        let (service, revisions, _) =
            f.start_inner(Some(deny.clone()), None, Some(journal.client()));
        let db = f.external(&revisions[0]);
        let mut request = service
            .prepare_source_wal(revisions[0].clone(), correlation(), deadline())
            .unwrap();
        let held = prepared(&mut request);
        deny.store(true, Ordering::Release);
        held.abort();
        wait(&request, |s| {
            matches!(s, CatalogSourceWalStatus::Unavailable(_))
        });
        busy(&db);
        service
            .submit(CatalogCommand::Remove {
                source_id: revisions[0].source_id().clone(),
                lifecycle_generation: 1,
            })
            .unwrap();
        wait(&request, |s| *s == CatalogSourceWalStatus::Retired);
        assert_eq!(credits(&service), (0, 0));
        db.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        assert_eq!(lease(&db).4, "released");
        // Removal is published only after the exact sealed acknowledgement has finalized.
        // A transient pending callback must be retried with that original carrier, not recreated.
        let end = Instant::now() + Duration::from_secs(2);
        let mut removed = false;
        while !removed {
            while let Ok(event) = service.try_recv_event() {
                if matches!(event,CatalogEvent::Removed { ref source_id, .. } if source_id==revisions[0].source_id())
                {
                    removed = true;
                }
            }
            if !removed {
                assert!(
                    Instant::now() < end,
                    "original retirement acknowledgement did not finalize"
                );
                service
                    .submit(CatalogCommand::Remove {
                        source_id: revisions[0].source_id().clone(),
                        lifecycle_generation: 1,
                    })
                    .unwrap();
                thread::sleep(Duration::from_millis(5));
            }
        }
        service.shutdown();
        assert!(journal.shutdown());
    }
    #[test]
    fn initial_audit_read_rollback_denial_never_returns_an_unconfirmed_writer_to_dispatch() {
        let f = Fixture::new();
        let deny = Arc::new(AtomicBool::new(false));
        let (service, revisions, _) = f.start(Some(deny.clone()));
        let db = f.external(&revisions[0]);
        let before = metadata(&db);
        deny.store(true, Ordering::Release);
        let request = service
            .prepare_source_wal(revisions[0].clone(), correlation(), deadline())
            .unwrap();
        wait(&request, |s| {
            matches!(s, CatalogSourceWalStatus::Unavailable(_))
        });
        assert_eq!(credits(&service).0, 1);
        assert_eq!(metadata(&db), before);
        service.submit(read(&revisions[0])).unwrap();
        thread::sleep(Duration::from_millis(50));
        assert_eq!(
            service.try_command_metrics().unwrap().dispatched,
            0,
            "a failed audit read rollback cannot expose ordinary SQL"
        );
        deny.store(false, Ordering::Release);
        wait(&request, |s| {
            matches!(s, CatalogSourceWalStatus::Rejected(_))
        });
        assert_eq!(credits(&service), (0, 0));
        service.shutdown();
    }
    #[test]
    fn complete_real_long_identity_context_exhausts_bytes_before_64_slots_without_leaking() {
        let mut f = Fixture::new();
        f.long_id = true;
        let (service, revisions, gate) = f.start(None);
        let pause = gate.pause_next();
        let mut requests = Vec::new();
        loop {
            match service.prepare_source_wal(revisions[0].clone(), correlation(), deadline()) {
                Ok(request) => requests.push(request),
                Err(CatalogSourceWalRejection::Full) => break,
                Err(error) => panic!("{error}"),
            }
        }
        let (slots, bytes) = credits(&service);
        assert_eq!(slots, requests.len());
        assert!(
            slots < 64,
            "complete real identity copies must charge aggregate bytes"
        );
        assert!(bytes <= 64 * 1024);
        {
            let state = service.admission.lock().unwrap();
            state.source_wal.close();
        }
        assert_eq!(credits(&service), (0, 0));
        drop(pause);
        service.shutdown();
    }

    #[test]
    fn correlation_constructor_rejects_zero_digest_and_nils_from_untrusted_id_decode() {
        let participant = ParticipantId::new();
        let claim = AdmissionClaimId::new();
        let sidecar = AdmissionClaimId::new();
        let nil: AdmissionClaimId =
            serde_json::from_str("\"00000000-0000-0000-0000-000000000000\"").unwrap();
        assert_eq!(
            CatalogSourceWalCorrelation::new([0; 32], participant, claim, sidecar).unwrap_err(),
            CatalogSourceWalRejection::InvalidContext
        );
        assert_eq!(
            CatalogSourceWalCorrelation::new([1; 32], participant, claim, claim).unwrap_err(),
            CatalogSourceWalRejection::InvalidContext
        );
        assert_eq!(
            CatalogSourceWalCorrelation::new([1; 32], participant, nil, sidecar).unwrap_err(),
            CatalogSourceWalRejection::InvalidContext
        );
        assert_eq!(
            CatalogSourceWalCorrelation::new([1; 32], participant, claim, nil).unwrap_err(),
            CatalogSourceWalRejection::InvalidContext
        );
        let nil_participant = ParticipantId::parse("00000000-0000-0000-0000-000000000000").unwrap();
        assert_eq!(
            CatalogSourceWalCorrelation::new([1; 32], nil_participant, claim, sidecar).unwrap_err(),
            CatalogSourceWalRejection::InvalidContext
        );
    }
    #[test]
    fn profile_namespace_loss_aborts_held_writer_without_renewal_or_retirement_claim() {
        let f = Fixture::new();
        let (service, revisions, _) = f.start(None);
        let db = f.external(&revisions[0]);
        let before = (lease(&db), metadata(&db));
        let mut request = service
            .prepare_source_wal(revisions[0].clone(), correlation(), deadline())
            .unwrap();
        let held = prepared(&mut request);
        let original = f.base.guard.profile_root().to_path_buf();
        let moved = original.with_extension("old-profile");
        fs::rename(&original, &moved).unwrap();
        wait(&request, |s| {
            *s == CatalogSourceWalStatus::Rejected(CatalogSourceWalRejection::Scope)
        });
        db.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        fs::rename(&moved, &original).unwrap();
        assert_eq!((lease(&db), metadata(&db)), before);
        assert_eq!(credits(&service), (0, 0));
        drop(held);
        service.shutdown();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn strict_scope_two_actual_sources_share_bounded_views_and_retain_bytes_after_abort() {
        let mut f = Fixture::new();
        f.local_prepared_authorities = true;
        let (service, revisions, _) = f.start(None);
        let a = f.external(&revisions[0]);
        let b = f.external(&revisions[1]);
        let before = [(lease(&a), metadata(&a)), (lease(&b), metadata(&b))];
        let common = deadline();
        let mut first = service
            .prepare_source_wal_with_binding(revisions[0].clone(), correlation(), common)
            .unwrap();
        let mut second = service
            .prepare_source_wal_with_binding(revisions[1].clone(), correlation(), common)
            .unwrap();
        let ha = prepared(&mut first);
        let hb = prepared(&mut second);
        let CatalogSourceWalStatus::Prepared(oa) = ha.status() else {
            panic!("actual held first scope")
        };
        let CatalogSourceWalStatus::Prepared(ob) = hb.status() else {
            panic!("actual held second scope")
        };
        let copy = oa.clone();
        assert_eq!(
            oa.scope_descriptor_bytes().unwrap().as_ptr(),
            copy.scope_descriptor_bytes().unwrap().as_ptr()
        );
        assert_ne!(oa.scope_descriptor_bytes(), ob.scope_descriptor_bytes());
        let d: serde_json::Value =
            serde_json::from_slice(oa.scope_descriptor_bytes().unwrap()).unwrap();
        assert_eq!(d["subject"]["registration"]["phase"], "prepared");
        assert_eq!(
            d["subject"]["database_identity"],
            revisions[0].database_identity()
        );
        assert!(credits(&service).1 <= CATALOG_WAL_MAX_CONTEXT_BYTES);
        busy(&a);
        busy(&b);
        assert_eq!(
            [(lease(&a), metadata(&a)), (lease(&b), metadata(&b))],
            before
        );
        assert_eq!(
            ha.acknowledge_settlement(),
            Err(CatalogSourceWalRejection::SettlementUnsupported)
        );
        drop(ha);
        drop(hb);
        wait(&first, |s| *s == CatalogSourceWalStatus::Aborted);
        wait(&second, |s| *s == CatalogSourceWalStatus::Aborted);
        assert_eq!(credits(&service).0, 0);
        let retained = credits(&service).1;
        assert!(retained > 0);
        drop(oa);
        assert_eq!(
            credits(&service).1,
            retained,
            "shared copy keeps exact credit"
        );
        drop(copy);
        assert!(credits(&service).1 < retained);
        drop(ob);
        assert_eq!(credits(&service), (0, 0));
        a.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        b.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        assert_eq!(
            [(lease(&a), metadata(&a)), (lease(&b), metadata(&b))],
            before
        );
        service.shutdown();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn strict_scope_missing_actual_registration_rejects_and_restores_original_owner() {
        let f = Fixture::new();
        let (service, revisions, _) = f.start(None);
        let a = f.external(&revisions[0]);
        let before = (lease(&a), metadata(&a));
        let request = service
            .prepare_source_wal_with_binding(revisions[0].clone(), correlation(), deadline())
            .unwrap();
        wait(&request, |s| {
            *s == CatalogSourceWalStatus::Rejected(CatalogSourceWalRejection::Scope)
        });
        assert!(request.status() != CatalogSourceWalStatus::Aborted);
        assert_eq!(credits(&service), (0, 0));
        a.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        assert_eq!((lease(&a), metadata(&a)), before);
        let mut unbound = service
            .prepare_source_wal(revisions[0].clone(), correlation(), deadline())
            .unwrap();
        let held = prepared(&mut unbound);
        assert!(
            matches!(held.status(), CatalogSourceWalStatus::Prepared(ref o) if o.scope_descriptor_bytes().is_none())
        );
        drop(held);
        wait(&unbound, |s| *s == CatalogSourceWalStatus::Aborted);
        service.shutdown();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn strict_scope_shutdown_releases_sql_owner_but_retains_copied_descriptor_credits() {
        let mut f = Fixture::new();
        f.local_prepared_authorities = true;
        let (service, revisions, _) = f.start(None);
        let a = f.external(&revisions[0]);
        let mut request = service
            .prepare_source_wal_with_binding(revisions[0].clone(), correlation(), deadline())
            .unwrap();
        let held = prepared(&mut request);
        let CatalogSourceWalStatus::Prepared(observation) = held.status() else {
            panic!("actual scope")
        };
        service.shutdown();
        assert_eq!(credits(&service).0, 0);
        assert!(credits(&service).1 > 0);
        assert!(observation.scope_descriptor_bytes().is_some());
        let accounting = service.source_wal_transport().accounting_probe_for_tests();
        drop(held);
        drop(request);
        drop(service);
        let retained = accounting
            .observe()
            .expect("report retains closed accounting");
        assert!(retained.2);
        assert!(retained.1 > 0);
        drop(observation);
        assert!(
            accounting.observe().is_none(),
            "final view releases accounting without a cycle"
        );
        a.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn strict_scope_actual_sidecar_substitution_is_detected_by_parked_owner() {
        let mut f = Fixture::new();
        f.local_prepared_authorities = true;
        let (service, revisions, _) = f.start(None);
        let mut request = service
            .prepare_source_wal_with_binding(revisions[0].clone(), correlation(), deadline())
            .unwrap();
        let held = prepared(&mut request);
        let path = f
            .base
            .guard
            .profile_root()
            .join(catalog_profile_child(revisions[0].source_id()))
            .join(format!("{DB_FILE_NAME}-shm"));
        let saved = path.with_extension("retained-original");
        std::fs::rename(&path, &saved).unwrap();
        let bytes = std::fs::metadata(&saved).unwrap().len();
        let replacement = std::fs::File::create(&path).unwrap();
        replacement.set_len(bytes).unwrap();
        wait(&request, |s| {
            *s == CatalogSourceWalStatus::Rejected(CatalogSourceWalRejection::Scope)
        });
        std::fs::remove_file(&path).unwrap();
        std::fs::rename(&saved, &path).unwrap();
        assert_eq!(credits(&service), (0, 0));
        drop(held);
        let mut again = service
            .prepare_source_wal_with_binding(revisions[0].clone(), correlation(), deadline())
            .unwrap();
        let held = prepared(&mut again);
        drop(held);
        wait(&again, |s| *s == CatalogSourceWalStatus::Aborted);
        service.shutdown();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn strict_scope_retained_reports_exhaust_capture_budget_and_return_every_temporary_credit() {
        let mut f = Fixture::new();
        f.local_prepared_authorities = true;
        let (service, revisions, _) = f.start(None);
        let a = f.external(&revisions[0]);
        let before = (lease(&a), metadata(&a));
        let mut reports = Vec::new();
        let mut refused = false;
        for _ in 0..20 {
            let prior = credits(&service);
            let mut request = service
                .prepare_source_wal_with_binding(revisions[0].clone(), correlation(), deadline())
                .unwrap();
            let status = wait(&request, |s| {
                matches!(
                    s,
                    CatalogSourceWalStatus::Prepared(_) | CatalogSourceWalStatus::Rejected(_)
                )
            });
            match status {
                CatalogSourceWalStatus::Prepared(o) => {
                    reports.push(o);
                    let held = request.try_prepared().unwrap();
                    drop(held);
                    wait(&request, |s| *s == CatalogSourceWalStatus::Aborted);
                    assert_eq!(credits(&service).0, 0);
                    assert!(credits(&service).1 > prior.1);
                }
                CatalogSourceWalStatus::Rejected(CatalogSourceWalRejection::Full) => {
                    assert_eq!(
                        credits(&service),
                        prior,
                        "rejected staging returned every temporary byte"
                    );
                    refused = true;
                    break;
                }
                other => panic!("unexpected strict budget outcome {other:?}"),
            }
            assert!(credits(&service).1 <= CATALOG_WAL_MAX_CONTEXT_BYTES);
        }
        assert!(
            refused,
            "actual retained report credits must bound subsequent capture"
        );
        assert!(!reports.is_empty());
        assert_eq!((lease(&a), metadata(&a)), before);
        a.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        drop(reports);
        assert_eq!(credits(&service), (0, 0));
        let mut again = service
            .prepare_source_wal_with_binding(revisions[0].clone(), correlation(), deadline())
            .unwrap();
        let held = prepared(&mut again);
        drop(held);
        wait(&again, |s| *s == CatalogSourceWalStatus::Aborted);
        service.shutdown();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn strict_scope_busy_unknown_vfs_and_original_expiry_keep_rows_and_return_owner_credits() {
        let mut f = Fixture::new();
        f.local_prepared_authorities = true;
        let (service, revisions, _) = f.start(None);
        let a = f.external(&revisions[0]);
        let before = (lease(&a), metadata(&a));
        a.execute_batch("BEGIN IMMEDIATE").unwrap();
        let request = service
            .prepare_source_wal_with_binding(revisions[0].clone(), correlation(), deadline())
            .unwrap();
        wait(&request, |s| {
            *s == CatalogSourceWalStatus::Rejected(CatalogSourceWalRejection::WalUnavailable)
        });
        a.execute_batch("ROLLBACK").unwrap();
        assert_eq!(credits(&service), (0, 0));
        let control = service.admission.lock().unwrap().source_wal.clone();
        control.set_unknown_wal_for_tests(true);
        let request = service
            .prepare_source_wal_with_binding(revisions[0].clone(), correlation(), deadline())
            .unwrap();
        wait(&request, |s| {
            *s == CatalogSourceWalStatus::Rejected(CatalogSourceWalRejection::WalUnavailable)
        });
        control.set_unknown_wal_for_tests(false);
        assert_eq!(credits(&service), (0, 0));
        let mut request = service
            .prepare_source_wal_with_binding(
                revisions[0].clone(),
                correlation(),
                Instant::now() + Duration::from_millis(500),
            )
            .unwrap();
        let held = prepared(&mut request);
        let CatalogSourceWalStatus::Prepared(o) = held.status() else {
            panic!("real bounded scope")
        };
        busy(&a);
        wait(&request, |s| {
            *s == CatalogSourceWalStatus::Rejected(CatalogSourceWalRejection::Deadline)
        });
        assert_eq!(credits(&service).0, 0);
        assert!(credits(&service).1 > 0);
        assert_eq!((lease(&a), metadata(&a)), before);
        drop(held);
        drop(o);
        assert_eq!(credits(&service), (0, 0));
        a.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        service.shutdown();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn strict_scope_failed_real_rollback_keeps_binding_credit_and_exclusion_until_confirmation() {
        let mut f = Fixture::new();
        f.local_prepared_authorities = true;
        let deny = Arc::new(AtomicBool::new(false));
        let (service, revisions, _) = f.start(Some(deny.clone()));
        let a = f.external(&revisions[0]);
        let before = (lease(&a), metadata(&a));
        let mut request = service
            .prepare_source_wal_with_binding(revisions[0].clone(), correlation(), deadline())
            .unwrap();
        let held = prepared(&mut request);
        let CatalogSourceWalStatus::Prepared(o) = held.status() else {
            panic!("actual scope")
        };
        deny.store(true, Ordering::Release);
        held.abort();
        wait(&request, |s| {
            *s == CatalogSourceWalStatus::Unavailable(CatalogSourceWalRejection::OwnerLost)
        });
        assert_eq!(credits(&service).0, 1);
        assert!(credits(&service).1 > 0);
        busy(&a);
        assert_eq!((lease(&a), metadata(&a)), before);
        deny.store(false, Ordering::Release);
        wait(&request, |s| *s == CatalogSourceWalStatus::Aborted);
        assert_eq!(credits(&service).0, 0);
        assert!(credits(&service).1 > 0);
        drop(held);
        drop(o);
        assert_eq!(credits(&service), (0, 0));
        a.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        service.shutdown();
    }
    #[test]
    fn library_requester_on_actual_catalog_retains_complete_diagnostic_views_until_last_copy() {
        let f = Fixture::new();
        let (service, revisions, _) = f.start(None);
        let transport = service.source_wal_transport();
        let mut request = transport
            .submit(revisions[0].clone(), correlation(), deadline())
            .unwrap();
        let held = prepared(&mut request);
        let CatalogSourceWalStatus::Prepared(observation) = held.status() else {
            panic!("actual native source")
        };
        let copy = observation.clone();
        assert_eq!(observation.revision(), &revisions[0]);
        assert!(observation.original_binding().is_none());
        drop(held);
        wait(&request, |s| *s == CatalogSourceWalStatus::Aborted);
        assert_eq!(credits(&service).0, 0);
        let retained = credits(&service).1;
        assert!(
            retained > 0,
            "complete revision/correlation/nonce memory remains charged"
        );
        drop(observation);
        assert_eq!(credits(&service).1, retained);
        service.shutdown();
        assert_eq!(credits(&service).1, retained);
        drop(copy);
        assert_eq!(credits(&service), (0, 0));
        f.external(&revisions[0])
            .execute_batch("BEGIN IMMEDIATE; ROLLBACK")
            .unwrap();
    }
    #[test]
    fn original_catalog_idle_checkpoint_excludes_genuinely_parked_other_source() {
        let f = Fixture::new();
        let (service, revisions, _) = f.start(None);
        let source = f.external(&revisions[0]);
        let pressured = f.external(&revisions[1]);
        pressured
            .pragma_update(None, "wal_autocheckpoint", 0)
            .unwrap();
        pressured
            .execute_batch(
                "CREATE TABLE idle_payload(value BLOB);
             INSERT INTO idle_payload VALUES(zeroblob(34603008));",
            )
            .unwrap();
        let main = std::path::PathBuf::from(pressured.path().unwrap());
        let before = std::fs::metadata(&main).unwrap().len();
        assert!(before < 34603008);
        let started = Instant::now();
        // Use the unchanged real 15-second worker interval and original 2-second preparation
        // deadline. Park shortly before idle admission, without extending any reservation.
        while started.elapsed() < Duration::from_secs(14) {
            thread::sleep(Duration::from_millis(10));
        }
        let mut request = service
            .prepare_source_wal(revisions[0].clone(), correlation(), deadline())
            .unwrap();
        let held = prepared(&mut request);
        busy(&source);
        assert_eq!(credits(&service).0, 1);
        while started.elapsed() < Duration::from_millis(15500) {
            thread::sleep(Duration::from_millis(10));
        }
        let after = std::fs::metadata(&main).unwrap().len();
        let still_parked = matches!(request.status(), CatalogSourceWalStatus::Prepared(_));
        drop(held);
        wait(&request, |status| {
            matches!(status, CatalogSourceWalStatus::Aborted)
        });
        service.shutdown();
        assert!(
            still_parked,
            "original bounded preparation did not span idle admission"
        );
        assert!(
            after < 34603008,
            "idle checkpoint crossed another source's parked preparation: {before} -> {after}"
        );
        source.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        assert_eq!(
            pressured
                .query_row("SELECT length(value) FROM idle_payload", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            34603008
        );
    }

    mod lease;
}
