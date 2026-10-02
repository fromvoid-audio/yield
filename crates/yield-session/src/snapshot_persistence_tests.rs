fn start_snapshot_fixture(
    temp: &TempDir,
) -> (Persistence, Option<AppRootGuard>, PersistenceProfileGuard) {
    let fixture = Persistence::start(
        Some(profile_root(temp)),
        Arc::new(Mutex::new(CatalogSlot::loading())),
        Arc::new(Mutex::new(VocabularySlot::loading())),
        Arc::new(MaintenanceAdmission::new()),
        true,
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !fixture.0.available() {
        assert!(fixture.0.unavailable.is_none(), "fixture unavailable");
        assert!(Instant::now() < deadline, "fixture initialization timeout");
        thread::sleep(Duration::from_millis(5));
    }
    fixture
}

fn dirty_snapshot_coordinator() -> Coordinator {
    let mut coordinator = Coordinator::new();
    coordinator
        .load(SourceRegistrySnapshot::new(vec![]))
        .unwrap();
    coordinator.revision = RegistryRevision(1);
    coordinator
}

fn await_snapshot_queue(persistence: &Persistence) {
    persistence
        .writer
        .as_ref()
        .unwrap()
        .load_source_registry_snapshot()
        .unwrap()
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .unwrap();
}

#[test]
fn snapshot_session_rejects_wrong_context_ack_without_clearing_dirty_revision() {
    let temp = TempDir::new().unwrap();
    let _base = ConfigBaseGuard::set(fs::canonicalize(temp.path()).unwrap());
    let (mut persistence, _root, _profile) = start_snapshot_fixture(&temp);
    let mut coordinator = dirty_snapshot_coordinator();
    queue_save(&mut persistence, &coordinator);
    await_snapshot_queue(&persistence);
    let wrong = GlobalMutationRequest::registry(
        persistence.writer.as_ref().unwrap().acquisition_id(),
        2,
        coordinator.snapshot(),
    )
    .unwrap();
    persistence.save.as_mut().unwrap().context = wrong.context().clone();
    let (events, receiver) = mpsc::channel();
    poll_save(
        &mut persistence,
        &mut coordinator,
        &events,
        &mut false,
        &mut HashSet::new(),
    );
    assert!(coordinator.is_dirty());
    assert!(persistence.unavailable.is_some());
    assert!(
        receiver
            .try_iter()
            .all(|event| !matches!(event, RegistryEvent::Persisted { .. }))
    );
    persistence.shutdown().unwrap();
}

#[test]
fn snapshot_session_coalesces_new_state_after_valid_stale_ack_and_confirms_only_latest() {
    let temp = TempDir::new().unwrap();
    let _base = ConfigBaseGuard::set(fs::canonicalize(temp.path()).unwrap());
    let (mut persistence, _root, _profile) = start_snapshot_fixture(&temp);
    let mut coordinator = dirty_snapshot_coordinator();
    queue_save(&mut persistence, &coordinator);
    await_snapshot_queue(&persistence);
    coordinator.revision = RegistryRevision(2);
    coordinator.current = vec![source(&root(&temp, "latest"), "latest-id")];
    let (events, receiver) = mpsc::channel();
    let mut reported = false;
    let mut pending = HashSet::new();
    poll_save(
        &mut persistence,
        &mut coordinator,
        &events,
        &mut reported,
        &mut pending,
    );
    assert!(coordinator.is_dirty());
    assert!(persistence.force_save);
    assert!(
        receiver
            .try_iter()
            .all(|event| !matches!(event, RegistryEvent::Persisted { .. }))
    );
    queue_save(&mut persistence, &coordinator);
    await_snapshot_queue(&persistence);
    poll_save(
        &mut persistence,
        &mut coordinator,
        &events,
        &mut reported,
        &mut pending,
    );
    assert!(!coordinator.is_dirty());
    assert!(!persistence.force_save);
    assert!(receiver.try_iter().any(
        |event| matches!(event, RegistryEvent::Persisted { revision } if revision.get() == 2)
    ));
    let stored = persistence
        .writer
        .as_ref()
        .unwrap()
        .load_source_registry_snapshot()
        .unwrap()
        .recv()
        .unwrap()
        .unwrap();
    assert_eq!(stored.as_slice()[0].id.as_str(), "latest-id");
    persistence.shutdown().unwrap();
}

#[test]
fn snapshot_session_disconnected_result_stays_unconfirmed_and_reloads_under_fresh_acquisition() {
    let temp = TempDir::new().unwrap();
    let _base = ConfigBaseGuard::set(fs::canonicalize(temp.path()).unwrap());
    let (mut persistence, root_guard, profile_guard) = start_snapshot_fixture(&temp);
    let old = persistence.writer.as_ref().unwrap().acquisition_id();
    let proposal =
        GlobalMutationRequest::registry(old.clone(), 1, SourceRegistrySnapshot::new(vec![]))
            .unwrap();
    let (sender, receiver) = mpsc::channel();
    drop(sender);
    persistence.save = Some(SaveInFlight {
        revision: RegistryRevision(1),
        context: proposal.context().clone(),
        receiver,
    });
    let mut coordinator = dirty_snapshot_coordinator();
    let (events, output) = mpsc::channel();
    poll_save(
        &mut persistence,
        &mut coordinator,
        &events,
        &mut false,
        &mut HashSet::new(),
    );
    assert!(coordinator.is_dirty());
    assert!(persistence.unavailable.is_some());
    assert!(
        output
            .try_iter()
            .all(|event| !matches!(event, RegistryEvent::Persisted { .. }))
    );
    persistence.shutdown().unwrap();
    drop(persistence);
    drop(root_guard);
    drop(profile_guard);
    let (mut fresh, _root, _profile) = start_snapshot_fixture(&temp);
    assert_ne!(old, fresh.writer.as_ref().unwrap().acquisition_id());
    let authoritative = fresh
        .writer
        .as_ref()
        .unwrap()
        .load_source_registry_snapshot()
        .unwrap()
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .unwrap();
    let mut reconciled = Coordinator::new();
    reconciled.load(authoritative).unwrap();
    assert!(!reconciled.is_dirty());
    assert!(reconciled.current.is_empty());
    // The obsolete desired proposal was not dispatched or automatically replayed on restart.
    fresh.shutdown().unwrap();
}

#[test]
fn snapshot_session_preferences_keep_wrong_or_disconnected_completions_unconfirmed() {
    for disconnected in [false, true] {
        let temp = TempDir::new().unwrap();
        let _base = ConfigBaseGuard::set(fs::canonicalize(temp.path()).unwrap());
        let (mut persistence, root_guard, profile_guard) = start_snapshot_fixture(&temp);
        let old = persistence.writer.as_ref().unwrap().acquisition_id();
        let mut preferences = PreferenceCoordinator::new();
        preferences.load_missing();
        preferences
            .apply_loaded_patch(
                PreferenceRequestId::from_raw(1),
                PreferencePatch::SetMotionEnabled(false),
            )
            .unwrap();
        assert!(preferences.is_dirty());
        queue_preference_save(&mut persistence, &preferences, true);
        await_snapshot_queue(&persistence);
        if disconnected {
            // Actual commit occurred, but its result is lost. This receiver provides no evidence.
            let (sender, receiver) = mpsc::channel();
            drop(sender);
            persistence.preference_save.as_mut().unwrap().receiver = receiver;
        } else {
            let wrong = GlobalMutationRequest::preferences(
                old.clone(),
                preferences.revision().get() + 1,
                preferences.snapshot().to_payload().unwrap(),
            )
            .unwrap();
            persistence.preference_save.as_mut().unwrap().context = wrong.context().clone();
        }
        let (events, output) = mpsc::channel();
        poll_preference_save(
            &mut persistence,
            &mut preferences,
            &events,
            &mut false,
            &mut HashSet::new(),
        );
        assert!(preferences.is_dirty());
        assert!(persistence.unavailable.is_some());
        assert!(
            output
                .try_iter()
                .all(|event| !matches!(event, PreferenceEvent::Persisted { .. }))
        );
        persistence.shutdown().unwrap();
        drop(persistence);
        drop(root_guard);
        drop(profile_guard);
        let (mut fresh, _root, _profile) = start_snapshot_fixture(&temp);
        assert_ne!(old, fresh.writer.as_ref().unwrap().acquisition_id());
        let payload = fresh
            .writer
            .as_ref()
            .unwrap()
            .load_ui_preferences_payload()
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap()
            .unwrap();
        // Fresh observation reconciles the actual committed state, without inventing the old ack.
        assert_eq!(payload, preferences.snapshot().to_payload().unwrap());
        fresh.shutdown().unwrap();
    }
}

#[test]
fn global_drain_session_propagates_owner_panic_after_reverse_cleanup() {
    let temp = TempDir::new().unwrap();
    let _base = ConfigBaseGuard::set(fs::canonicalize(temp.path()).unwrap());
    let (mut persistence, _root, _profile) = start_snapshot_fixture(&temp);
    let lost = persistence
        .writer
        .as_ref()
        .unwrap()
        .panic_owner_for_test()
        .unwrap();
    assert!(lost.recv_timeout(Duration::from_secs(2)).is_err());
    assert!(matches!(
        persistence.shutdown(),
        Err(SessionError::GlobalWriterDrain(
            GlobalLibraryWriterDrainOutcome::WorkerPanicked
        ))
    ));
    assert!(persistence.writer.is_none());
    assert!(persistence.catalog.is_none());
    assert!(persistence.journal.is_none());
    assert!(persistence.writable.is_none());
}

#[test]
fn global_drain_session_joins_panicked_worker_and_retains_failure() {
    let temp = TempDir::new().unwrap();
    let _base = ConfigBaseGuard::set(fs::canonicalize(temp.path()).unwrap());
    let path = profile_root(&temp);
    let mut service =
        SourceRegistryService::start_portable_registry_for_test(path.clone()).unwrap();
    wait_for_event(&service, |event| {
        matches!(event, RegistryEvent::Snapshot { .. })
    });
    service
        .submit_command(super::super::Command::PanicForTest)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !service.worker.as_ref().unwrap().is_finished() {
        assert!(Instant::now() < deadline, "session worker panic timeout");
        thread::sleep(Duration::from_millis(5));
    }
    assert!(matches!(
        service.shutdown(),
        Err(ShutdownError::WorkerPanicked)
    ));
    assert!(
        service.worker.is_none(),
        "failed acknowledgement must still join"
    );
    assert!(matches!(
        service.shutdown(),
        Err(ShutdownError::WorkerPanicked)
    ));
    let mut fresh = SourceRegistryService::start_portable_registry_for_test(path).unwrap();
    wait_for_event(&fresh, |event| {
        matches!(event, RegistryEvent::Snapshot { .. })
    });
    fresh.shutdown().unwrap();
}
