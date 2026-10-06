use super::*;

#[test]
fn reference_wal_reacquired_sources_and_profile_replay_without_marker() {
    let mut f = ReferenceFixture::new();
    f.seed();
    let request = f.request(1);
    let bytes = request.proposal_bytes().to_vec();
    let committed = f.apply(request.clone()).unwrap();
    f.db().execute_batch("UPDATE harvest_files SET note='later authored edit',harvest_state='ignored',ignored_at=77 WHERE source_id='destination'; INSERT INTO harvest_files(source_id,relative_path,content_hash,harvest_state,discovered_at) VALUES('origin','one.wav','later','ignored',99);").unwrap();
    f.restart();
    assert!(matches!(
        f.apply(request),
        Err(LibraryError::ReferenceStaleOwner)
    ));
    let (origin, destination) = f.fences();
    let restored = ReferenceRekeyRequest::restore(f.owner(), &bytes, origin, destination).unwrap();
    stop_global(&mut f);
    let mut database =
        LibraryDatabase::open_for_profile_guard(f.guard.as_ref().unwrap(), None).unwrap();
    assert!(!database.ordinary_commit_pending_for_test());
    let replay = database
        .apply_reference_rekey(f.owner(), &restored)
        .unwrap();
    assert!(matches!(replay, ReferenceRekeyResult::Replayed(_)));
    assert_eq!(replay.receipt(), committed.receipt());
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
    assert_eq!(
        f.count(
            "SELECT COUNT(*) FROM harvest_files WHERE source_id='origin' AND content_hash='later'"
        ),
        1
    );
    assert_eq!(f.count("SELECT COUNT(*) FROM reference_receipts"), 1);
    assert_eq!(f.count("SELECT COUNT(*) FROM harvest_derivations"), 3);
}
