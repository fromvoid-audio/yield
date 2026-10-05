use super::*;

#[test]
fn harvest_copy_wal_reacquired_original_sources_and_profile_replay_without_marker() {
    let mut f = Fixture::new();
    let request = f.request();
    let bytes = request.proposal_bytes().to_vec();
    let operation = request.operation();
    let harvest = request.work_key();
    let enrollment_bytes = f.enrollment.proposal_bytes().to_vec();
    stop_global(&mut f);
    let mut database = LibraryDatabase::open_for_profile_guard(f.guard(), None).unwrap();
    let committed = database
        .apply_harvest_copy(f.guard().owner_identity(), &request)
        .unwrap();
    assert!(database.ordinary_commit_pending_for_test());
    database.connection.execute("UPDATE harvest_files SET note='later authored edit',harvest_state='ignored',ignored_at=77 WHERE source_id='destination'", []).unwrap();
    drop(database);
    let original_owner = f.guard().owner_identity().to_string();
    let mut journal = f.journal.take().unwrap();
    assert!(journal.shutdown());
    drop(journal);
    for source in f.sources.take().unwrap() {
        source.into_catalog_database().close().unwrap();
    }
    f.guard.take();
    let guard = WritableProfileGuard::acquire_current().unwrap();
    assert_ne!(guard.owner_identity(), original_owner);
    let sources = ["origin", "destination"].map(|id| {
        let index = usize::from(id == "destination");
        let source = SourceDatabase::open_for_catalog_with_profile_guard(
            f.roots[index].path(),
            &guard,
            Path::new(id),
        )
        .unwrap();
        SourceWriter::new_for_scan(source, SourceId::from_string(id), 2).unwrap()
    });
    let journal = OperationJournalOwner::start_without_capacity_for_test(
        OperationJournalStore::open(&guard).unwrap(),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while journal.status() != OperationJournalOwnerStatus::Available {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(request.validate_live(false).is_err());
    let family = journal
        .client()
        .observe_copy_intent(operation, harvest)
        .unwrap()
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap()
        .unwrap();
    let enrollment = ManifestEnrollmentRequest::restore(&enrollment_bytes, &sources[1]).unwrap();
    let enrollment = sources[1]
        .replay_manifest_enrollment(&enrollment)
        .unwrap()
        .unwrap()
        .receipt;
    f.guard = Some(guard);
    f.sources = Some(sources);
    f.journal = Some(journal);
    f.family = family;
    f.enrollment = enrollment;
    let restored = f.restore(&bytes).unwrap();
    let mut database = LibraryDatabase::open_for_profile_guard(f.guard(), None).unwrap();
    assert!(!database.ordinary_commit_pending_for_test());
    let replay = database
        .apply_harvest_copy(f.guard().owner_identity(), &restored)
        .unwrap();
    assert!(matches!(replay, HarvestCopyResult::Replayed(_)));
    assert_eq!(replay.receipt().sequence(), committed.receipt().sequence());
    assert_eq!(replay.receipt().edge_id(), committed.receipt().edge_id());
    assert!(!database.ordinary_commit_pending_for_test());
    let later: (String, String, i64) = database
        .connection
        .query_row(
            "SELECT note,harvest_state,ignored_at FROM harvest_files WHERE source_id='destination'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(later, ("later authored edit".into(), "ignored".into(), 77));
    assert_eq!(f.count("harvest_copy_receipts"), 1);
    assert_eq!(f.count("harvest_derivations"), 1);
}
