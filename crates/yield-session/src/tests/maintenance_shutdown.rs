use super::*;

#[test]
fn session_reports_original_maintenance_join_panic_after_remaining_owners_finish() {
    original_maintenance_completion(true, false);
}

#[test]
fn session_global_failure_precedes_original_maintenance_join_panic() {
    original_maintenance_completion(true, true);
}

#[test]
fn session_original_maintenance_healthy_join_and_repeat_remain_successful() {
    original_maintenance_completion(false, false);
}

fn original_maintenance_completion(maintenance_panic: bool, global_panic: bool) {
    let temp = TempDir::new().expect("config base");
    let _base = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
    let catalog_slot = Arc::new(Mutex::new(CatalogSlot::loading()));
    let vocabulary_slot = Arc::new(Mutex::new(VocabularySlot::loading()));
    let admission = Arc::new(MaintenanceAdmission::new());
    let (mut persistence, _root_guard, _profile_guard) = Persistence::start(
        Some(profile_root(&temp)),
        Arc::clone(&catalog_slot),
        Arc::clone(&vocabulary_slot),
        Arc::clone(&admission),
        true,
    );
    assert!(persistence.unavailable.is_none());
    let catalog = persistence.catalog.as_ref().unwrap().clone();
    let journal_client = persistence.journal.as_ref().unwrap().client();
    let gate = persistence
        .maintenance
        .arm_completion_for_test(maintenance_panic);
    let reservation = admission.reserve(MaintenanceKind::StorageUsage).unwrap();
    let cancelled = reservation.cancel_token();
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    persistence
        .maintenance
        .start_storage(persistence.writable.as_ref(), reservation, result_tx);
    assert!(gate.wait_entered(Duration::from_secs(5)));
    let _original_operation_result = result_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    let global_failure = if global_panic {
        let writer = persistence.writer.as_ref().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !matches!(writer.status(), GlobalLibraryWriterStatus::Available) {
            assert!(Instant::now() < deadline, "original Global startup");
            thread::yield_now();
        }
        Some(writer.panic_owner_for_test().unwrap())
    } else {
        None
    };
    let release = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !cancelled.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::yield_now();
        }
        let original_shutdown_requested = cancelled.load(Ordering::Acquire);
        gate.release();
        original_shutdown_requested
    });
    let result = persistence.shutdown();
    assert!(
        release.join().unwrap(),
        "original maintenance cancellation observed before release"
    );
    if global_panic {
        assert_eq!(
            result,
            Err(SessionError::GlobalWriterDrain(
                GlobalLibraryWriterDrainOutcome::WorkerPanicked
            ))
        );
        assert!(
            global_failure
                .unwrap()
                .recv_timeout(Duration::from_secs(2))
                .is_err()
        );
    } else if maintenance_panic {
        assert!(
            matches!(result, Err(SessionError::Persistence(ref reason)) if reason.contains("maintenance shutdown") && reason.contains("panicked")),
            "{result:?}"
        );
        assert_eq!(
            persistence.shutdown(),
            result,
            "original maintenance panic remains retained"
        );
    } else {
        assert!(result.is_ok());
        assert!(persistence.shutdown().is_ok());
    }
    assert!(persistence.catalog.is_none());
    assert!(persistence.writer.is_none());
    assert!(persistence.journal.is_none());
    assert!(persistence.writable.is_none());
    assert!(journal_client.get(OperationId::new()).is_err());
    assert_eq!(
        catalog.submit(CatalogCommand::ReadSnapshot {
            source_id: SourceId::from_string("late-maintenance-failure"),
            expected_revision: None,
        }),
        Err(yield_catalog::CatalogSubmitError::Closed)
    );
    drop(journal_client);
    drop(catalog);
    drop(catalog_slot);
    drop(vocabulary_slot);
    let profile = WritableProfileGuard::acquire_current().expect("all original owners joined");
    let journal = OperationJournalStore::open(&profile).expect("original journal child released");
    drop(journal);
    drop(profile);
}
