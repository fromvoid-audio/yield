use super::*;

#[test]
fn session_reports_original_catalog_failure_after_remaining_owner_joins() {
    let temp = TempDir::new().expect("config base");
    let _base = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
    let catalog_slot = Arc::new(Mutex::new(CatalogSlot::loading()));
    let vocabulary_slot = Arc::new(Mutex::new(VocabularySlot::loading()));
    let (mut persistence, _root_guard, _profile_guard) = Persistence::start(
        Some(profile_root(&temp)), Arc::clone(&catalog_slot), Arc::clone(&vocabulary_slot),
        Arc::new(MaintenanceAdmission::new()), true,
    );
    assert!(persistence.unavailable.is_none());
    let catalog = persistence.catalog.as_ref().unwrap().clone();
    let fault = catalog.arm_original_worker_panic().expect("original worker fixture");
    assert!(fault.wait_entered(Duration::from_secs(5)));
    let release = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !fault.shutdown_requested() && Instant::now() < deadline { thread::yield_now(); }
        let closing = fault.shutdown_requested();
        fault.release();
        closing
    });
    let result = persistence.shutdown();
    assert!(release.join().unwrap(), "original Catalog Closing observed");
    assert!(matches!(result, Err(SessionError::Persistence(ref reason))
        if reason.contains("catalog shutdown") && reason.contains("WorkerPanicked")), "{result:?}");
    assert!(persistence.catalog.is_none());
    assert!(persistence.writer.is_none());
    assert!(persistence.journal.is_none());
    assert!(persistence.writable.is_none());
    assert_eq!(catalog.submit(CatalogCommand::ReadSnapshot {
        source_id: SourceId::from_string("late-catalog-failure"), expected_revision: None,
    }), Err(yield_catalog::CatalogSubmitError::Closed));
    drop(catalog);
    drop(catalog_slot);
    drop(vocabulary_slot);
    let profile = WritableProfileGuard::acquire_current().expect("all original profile owners joined");
    let journal = OperationJournalStore::open(&profile).expect("journal child lease released");
    drop(journal);
    drop(profile);
}
