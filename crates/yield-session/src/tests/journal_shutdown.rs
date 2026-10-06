use super::*;

#[test]
fn session_reports_original_journal_failure_during_final_join() {
    original_failure(false);
}

#[test]
fn session_global_failure_still_joins_original_failed_journal() {
    original_failure(true);
}

fn original_failure(global_panic: bool) {
    let temp = TempDir::new().expect("config base");
    let _base = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
    let root = profile_root(&temp);
    let catalog_slot = Arc::new(Mutex::new(CatalogSlot::loading()));
    let vocabulary_slot = Arc::new(Mutex::new(VocabularySlot::loading()));
    let (mut persistence, _root_guard, _profile_guard) = Persistence::start(
        Some(root),
        Arc::clone(&catalog_slot),
        Arc::clone(&vocabulary_slot),
        Arc::new(MaintenanceAdmission::new()),
        true,
    );
    assert!(persistence.unavailable.is_none());
    let catalog = persistence
        .catalog
        .as_ref()
        .expect("original catalog")
        .clone();
    let mut initial = persistence.journal.take().expect("original journal");
    assert!(initial.shutdown());
    drop(initial);
    let store = OperationJournalStore::open(persistence.writable.as_ref().unwrap())
        .expect("original profile journal");
    let (journal, ready, release) = OperationJournalOwner::start_with_worker_panic_for_test(store);
    let client = journal.client();
    persistence.journal = Some(journal);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !matches!(client.status(), OperationJournalOwnerStatus::Available) {
        assert!(Instant::now() < deadline, "journal startup");
        thread::yield_now();
    }
    let mut request = client
        .get(OperationId::new())
        .expect("accepted original command");
    ready.wait();
    assert!(persistence.status_error().is_none());
    let global_failure = if global_panic {
        let writer = persistence.writer.as_ref().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !matches!(writer.status(), GlobalLibraryWriterStatus::Available) {
            assert!(Instant::now() < deadline, "global startup");
            thread::yield_now();
        }
        Some(
            writer
                .panic_owner_for_test()
                .expect("original global panic command"),
        )
    } else {
        None
    };
    let observed = client.clone();
    let release_thread = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut closing = false;
        while Instant::now() < deadline {
            if matches!(observed.status(), OperationJournalOwnerStatus::Closing) {
                closing = true;
                break;
            }
            thread::yield_now();
        }
        // Always release the owned worker so a failed timing assertion cannot strand it.
        release.wait();
        closing
    });
    let result = persistence.shutdown();
    assert!(release_thread.join().expect("release thread"));
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
    } else {
        assert!(
            matches!(result, Err(SessionError::Persistence(ref reason))
            if reason.contains("journal shutdown") && reason.contains("panicked")),
            "{result:?}"
        );
    }
    assert!(matches!(
        client.status(),
        OperationJournalOwnerStatus::Unavailable { .. }
    ));
    assert!(client.get(OperationId::new()).is_err());
    assert!(request.recv_timeout(Duration::from_secs(2)).is_err());
    assert!(persistence.catalog.is_none());
    assert!(persistence.writer.is_none());
    assert!(persistence.journal.is_none());
    assert!(persistence.writable.is_none());
    assert_eq!(
        catalog.submit(CatalogCommand::ReadSnapshot {
            source_id: SourceId::from_string("late-journal-failure"),
            expected_revision: None,
        }),
        Err(yield_catalog::CatalogSubmitError::Closed)
    );
    drop(catalog);
    drop(catalog_slot);
    drop(vocabulary_slot);
    let owner = WritableProfileGuard::acquire_current().expect("profile released after all joins");
    let reopened = OperationJournalStore::open(&owner).expect("journal child lease released");
    drop(reopened);
    drop(owner);
}

#[test]
fn session_healthy_journal_shutdown_retains_repeat_success() {
    let temp = TempDir::new().expect("config base");
    let _base = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
    let mut session = SourceRegistryService::start_portable_registry_for_test(profile_root(&temp))
        .expect("session startup");
    wait_for_snapshot(&session, |snapshot| snapshot.as_slice().is_empty());
    wait_for_preference_snapshot(&session, |_| true);
    assert_eq!(session.shutdown(), Ok(()));
    assert_eq!(session.shutdown(), Ok(()));
    assert!(matches!(
        session.add_root(root(&temp, "closed")),
        Err(SubmitError::Closed)
    ));
    let profile = WritableProfileGuard::acquire_current().expect("original profile released");
    let journal = OperationJournalStore::open(&profile).expect("original child lease released");
    drop(journal);
    drop(profile);
}
