use super::*;

#[test]
fn public_session_caches_original_catalog_failure_after_join() {
    let temp = TempDir::new().expect("config base");
    let _base = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
    let mut session =
        SourceRegistryService::start_portable_registry_for_test(profile_root(&temp)).unwrap();
    wait_for_snapshot(&session, |snapshot| snapshot.as_slice().is_empty());
    wait_for_preference_snapshot(&session, |_| true);
    let catalog =
        catalog::original_service_for_test(&session.catalog).expect("actual installed Catalog");
    let fault = catalog.arm_original_worker_panic().unwrap();
    assert!(fault.wait_entered(Duration::from_secs(5)));
    let release = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !fault.shutdown_requested() && Instant::now() < deadline {
            thread::yield_now();
        }
        let closing = fault.shutdown_requested();
        fault.release();
        closing
    });
    let first = session.shutdown();
    assert!(release.join().unwrap());
    let second = session.shutdown();
    match (first, second) {
        (
            Err(ShutdownError::Session(SessionError::Persistence(first))),
            Err(ShutdownError::Session(SessionError::Persistence(second))),
        ) => {
            assert!(first.contains("catalog shutdown") && first.contains("WorkerPanicked"));
            assert_eq!(first, second);
        }
        results => panic!("actual original Catalog error must remain cached: {results:?}"),
    }
    assert!(matches!(
        session.add_root(root(&temp, "closed")),
        Err(SubmitError::Closed)
    ));
    assert_eq!(
        catalog.shutdown_with_report().outcome(),
        yield_catalog::CatalogThreadJoinOutcome::WorkerPanicked
    );
    drop(catalog);
    drop(session);
    let profile =
        WritableProfileGuard::acquire_current().expect("public shutdown joined original owners");
    let journal = OperationJournalStore::open(&profile).expect("original journal child released");
    drop(journal);
    drop(profile);
}

#[test]
fn session_reports_original_catalog_failure_after_remaining_owner_joins() {
    original_failure(false, false);
}

#[test]
fn session_global_error_precedes_catalog_panic_after_all_joins() {
    original_failure(true, false);
}

#[test]
fn session_journal_error_precedes_catalog_panic_after_all_joins() {
    original_failure(false, true);
}

fn original_failure(global_panic: bool, journal_panic: bool) {
    let temp = TempDir::new().expect("config base");
    let _base = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
    let catalog_slot = Arc::new(Mutex::new(CatalogSlot::loading()));
    let vocabulary_slot = Arc::new(Mutex::new(VocabularySlot::loading()));
    let (mut persistence, _root_guard, _profile_guard) = Persistence::start(
        Some(profile_root(&temp)),
        Arc::clone(&catalog_slot),
        Arc::clone(&vocabulary_slot),
        Arc::new(MaintenanceAdmission::new()),
        true,
    );
    assert!(persistence.unavailable.is_none());
    let catalog = persistence.catalog.as_ref().unwrap().clone();
    let fault = catalog
        .arm_original_worker_panic()
        .expect("original worker fixture");
    assert!(fault.wait_entered(Duration::from_secs(5)));
    let global_failure = if global_panic {
        let writer = persistence.writer.as_ref().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !matches!(writer.status(), GlobalLibraryWriterStatus::Available) {
            assert!(Instant::now() < deadline, "original Global startup");
            thread::yield_now();
        }
        Some(
            writer
                .panic_owner_for_test()
                .expect("original Global panic command"),
        )
    } else {
        None
    };
    let journal_failure = if journal_panic {
        let mut initial = persistence.journal.take().unwrap();
        assert!(initial.shutdown());
        drop(initial);
        let store = OperationJournalStore::open(persistence.writable.as_ref().unwrap()).unwrap();
        let (owner, ready, release) =
            OperationJournalOwner::start_with_worker_panic_for_test(store);
        let client = owner.client();
        persistence.journal = Some(owner);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !matches!(client.status(), OperationJournalOwnerStatus::Available) {
            assert!(Instant::now() < deadline, "original Journal startup");
            thread::yield_now();
        }
        let request = client
            .get(OperationId::new())
            .expect("original accepted command");
        ready.wait();
        let observed = client.clone();
        let releaser = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !matches!(observed.status(), OperationJournalOwnerStatus::Closing)
                && Instant::now() < deadline
            {
                thread::yield_now();
            }
            let closing = matches!(observed.status(), OperationJournalOwnerStatus::Closing);
            release.wait();
            closing
        });
        Some((client, request, releaser))
    } else {
        None
    };
    let release = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !fault.shutdown_requested() && Instant::now() < deadline {
            thread::yield_now();
        }
        let closing = fault.shutdown_requested();
        fault.release();
        closing
    });
    let result = persistence.shutdown();
    assert!(release.join().unwrap(), "original Catalog Closing observed");
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
    } else if journal_panic {
        assert!(
            matches!(result, Err(SessionError::Persistence(ref reason))
            if reason.contains("journal shutdown") && reason.contains("panicked")),
            "{result:?}"
        );
    } else {
        assert!(
            matches!(result, Err(SessionError::Persistence(ref reason))
            if reason.contains("catalog shutdown") && reason.contains("WorkerPanicked")),
            "{result:?}"
        );
    }
    if let Some((client, mut request, releaser)) = journal_failure {
        assert!(releaser.join().unwrap());
        assert!(matches!(
            client.status(),
            OperationJournalOwnerStatus::Unavailable { .. }
        ));
        assert!(request.recv_timeout(Duration::from_secs(2)).is_err());
    }
    assert_eq!(
        catalog.shutdown_with_report().outcome(),
        yield_catalog::CatalogThreadJoinOutcome::WorkerPanicked
    );
    assert!(persistence.catalog.is_none());
    assert!(persistence.writer.is_none());
    assert!(persistence.journal.is_none());
    assert!(persistence.writable.is_none());
    assert_eq!(
        catalog.submit(CatalogCommand::ReadSnapshot {
            source_id: SourceId::from_string("late-catalog-failure"),
            expected_revision: None,
        }),
        Err(yield_catalog::CatalogSubmitError::Closed)
    );
    drop(catalog);
    drop(catalog_slot);
    drop(vocabulary_slot);
    let profile =
        WritableProfileGuard::acquire_current().expect("all original profile owners joined");
    let journal = OperationJournalStore::open(&profile).expect("journal child lease released");
    drop(journal);
    drop(profile);
}
