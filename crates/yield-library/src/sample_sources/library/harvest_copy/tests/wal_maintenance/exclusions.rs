use super::*;

#[test]
fn harvest_copy_wal_pending_bit_distinguishes_commit_replay_and_sql_refusals() {
    let mut f = Fixture::new();
    let request = f.request();
    stop_global(&mut f);
    let mut database = LibraryDatabase::open_for_profile_guard(f.guard(), None).unwrap();
    assert!(!database.ordinary_commit_pending_for_test());
    let mut foreign = request.clone();
    foreign.owner = Uuid::new_v4();
    assert!(matches!(
        database.apply_harvest_copy(f.guard().owner_identity(), &foreign),
        Err(LibraryError::HarvestCopyStaleOwner)
    ));
    assert!(!database.ordinary_commit_pending_for_test());
    database.connection.execute_batch("CREATE TRIGGER fail_copy_maintenance BEFORE INSERT ON harvest_copy_receipts BEGIN SELECT RAISE(ABORT,'fixture'); END").unwrap();
    assert!(
        database
            .apply_harvest_copy(f.guard().owner_identity(), &request)
            .is_err()
    );
    assert!(!database.ordinary_commit_pending_for_test());
    assert_eq!(f.count("harvest_derivations"), 0);
    assert_eq!(f.count("harvest_copy_receipts"), 0);
    database
        .connection
        .execute_batch("DROP TRIGGER fail_copy_maintenance")
        .unwrap();
    database.connection.commit_hook(Some(|| true));
    assert!(
        database
            .apply_harvest_copy(f.guard().owner_identity(), &request)
            .is_err()
    );
    assert!(!database.ordinary_commit_pending_for_test());
    assert_eq!(f.count("harvest_copy_receipts"), 0);
    database.connection.commit_hook(None::<fn() -> bool>);
    let result = database
        .apply_harvest_copy(f.guard().owner_identity(), &request)
        .unwrap();
    assert!(matches!(result, HarvestCopyResult::Committed(_)));
    assert!(database.ordinary_commit_pending_for_test());
    database.maintain_after_ordinary_commit(f.guard());
    assert!(!database.ordinary_commit_pending_for_test());
    let replay = database
        .apply_harvest_copy(f.guard().owner_identity(), &request)
        .unwrap();
    assert!(matches!(replay, HarvestCopyResult::Replayed(_)));
    assert_eq!(replay.receipt().sequence(), result.receipt().sequence());
    assert!(
        !database.ordinary_commit_pending_for_test(),
        "direct bit excludes throttle masking"
    );
    assert_eq!(f.count("harvest_copy_receipts"), 1);
    assert_eq!(f.count("harvest_derivations"), 1);
}

#[test]
fn harvest_copy_wal_pending_bit_excludes_capacity_changed_source_and_ended_journal() {
    for fill in [
        "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<8192) INSERT INTO harvest_copy_receipts(command_key,payload,result,identity,owner) SELECT 'capacity-'||x,x'7b7d',x'7b7d',zeroblob(32),'00000000-0000-0000-0000-000000000001' FROM n;",
        "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<256) INSERT INTO harvest_copy_receipts(command_key,payload,result,identity,owner) SELECT 'capacity-'||x,zeroblob(65536),x'7b7d',zeroblob(32),'00000000-0000-0000-0000-000000000001' FROM n;",
    ] {
        let mut f = Fixture::new();
        let request = f.request();
        stop_global(&mut f);
        let mut database = LibraryDatabase::open_for_profile_guard(f.guard(), None).unwrap();
        // Synthetic rows exercise the existing retention ceiling only, never physical admission.
        database.connection.execute_batch(fill).unwrap();
        assert!(matches!(
            database.apply_harvest_copy(f.guard().owner_identity(), &request),
            Err(LibraryError::HarvestCopyReceiptCapacity)
        ));
        assert!(!database.ordinary_commit_pending_for_test());
        database
            .connection
            .execute_batch("DELETE FROM harvest_copy_receipts")
            .unwrap();
        f.source_connection(0)
            .execute("UPDATE wav_files SET content_hash=?1", ["b".repeat(64)])
            .unwrap();
        assert!(
            database
                .apply_harvest_copy(f.guard().owner_identity(), &request)
                .is_err()
        );
        assert!(!database.ordinary_commit_pending_for_test());
        f.source_connection(0)
            .execute("UPDATE wav_files SET content_hash=?1", ["a".repeat(64)])
            .unwrap();
        let mut journal = f.journal.take().unwrap();
        assert!(journal.shutdown());
        assert!(
            database
                .apply_harvest_copy(f.guard().owner_identity(), &request)
                .is_err()
        );
        assert!(!database.ordinary_commit_pending_for_test());
        assert_eq!(f.count("harvest_copy_receipts"), 0);
    }
}

#[test]
fn harvest_copy_wal_source_change_at_confirmed_commit_keeps_marker_and_uncertainty() {
    let mut f = Fixture::new();
    let request = f.request();
    let bytes = request.proposal_bytes().to_vec();
    stop_global(&mut f);
    let mut database = LibraryDatabase::open_for_profile_guard(f.guard(), None).unwrap();
    let source = f.source_connection(0);
    database.connection.commit_hook(Some(move || {
        source
            .execute("UPDATE wav_files SET content_hash=?1", ["b".repeat(64)])
            .unwrap();
        false
    }));
    assert!(matches!(
        database.apply_harvest_copy(f.guard().owner_identity(), &request),
        Err(LibraryError::HarvestCopyCommitUncertain)
    ));
    assert!(database.ordinary_commit_pending_for_test());
    assert_eq!(f.count("harvest_copy_receipts"), 1);
    assert_eq!(f.count("harvest_derivations"), 1);
    database.connection.commit_hook(None::<fn() -> bool>);
    database.maintain_after_ordinary_commit(f.guard());
    let restored = f.restore(&bytes).unwrap();
    assert!(matches!(
        database
            .apply_harvest_copy(f.guard().owner_identity(), &restored)
            .unwrap(),
        HarvestCopyResult::Replayed(_)
    ));
    assert!(!database.ordinary_commit_pending_for_test());
    assert_eq!(f.count("harvest_derivations"), 1);
}
