use super::*;

#[test]
fn reference_wal_pending_bit_distinguishes_commit_replay_and_sql_refusals() {
    let mut f = ReferenceFixture::new();
    f.seed();
    let request = f.request(1);
    stop_global(&mut f);
    let mut database =
        LibraryDatabase::open_for_profile_guard(f.guard.as_ref().unwrap(), None).unwrap();
    assert!(!database.ordinary_commit_pending_for_test());
    assert!(matches!(
        database.apply_reference_rekey(&uuid::Uuid::new_v4().to_string(), &request),
        Err(LibraryError::ReferenceStaleOwner)
    ));
    assert!(!database.ordinary_commit_pending_for_test());
    database.connection.execute_batch("CREATE TRIGGER fail_reference_maintenance BEFORE INSERT ON reference_receipts BEGIN SELECT RAISE(ABORT,'fixture'); END").unwrap();
    assert!(database.apply_reference_rekey(f.owner(), &request).is_err());
    assert!(!database.ordinary_commit_pending_for_test());
    assert_eq!(f.count("SELECT COUNT(*) FROM reference_receipts"), 0);
    assert_eq!(
        f.count("SELECT COUNT(*) FROM harvest_files WHERE source_id='origin'"),
        1
    );
    database
        .connection
        .execute_batch("DROP TRIGGER fail_reference_maintenance")
        .unwrap();
    database.connection.commit_hook(Some(|| true));
    assert!(database.apply_reference_rekey(f.owner(), &request).is_err());
    assert!(!database.ordinary_commit_pending_for_test());
    assert_eq!(f.count("SELECT COUNT(*) FROM reference_receipts"), 0);
    assert_eq!(
        f.count("SELECT COUNT(*) FROM harvest_files WHERE source_id='origin'"),
        1
    );
    assert_eq!(
        f.count("SELECT COUNT(*) FROM harvest_files WHERE source_id='destination'"),
        0
    );
    assert_eq!(f.count("SELECT COUNT(*) FROM harvest_derivations WHERE parent_source_id='origin' OR child_source_id='origin'"), 3);
    database.connection.commit_hook(None::<fn() -> bool>);
    let result = database.apply_reference_rekey(f.owner(), &request).unwrap();
    assert!(matches!(result, ReferenceRekeyResult::Committed(_)));
    assert!(database.ordinary_commit_pending_for_test());
    database.maintain_after_ordinary_commit(f.guard.as_ref().unwrap());
    assert!(!database.ordinary_commit_pending_for_test());
    let replay = database.apply_reference_rekey(f.owner(), &request).unwrap();
    assert!(matches!(replay, ReferenceRekeyResult::Replayed(_)));
    assert_eq!(replay.receipt().sequence(), result.receipt().sequence());
    assert!(
        !database.ordinary_commit_pending_for_test(),
        "direct bit excludes throttle masking"
    );
    let (old, new) = ReferenceFixture::keys();
    let (origin, destination) = f.fences();
    let snapshot = database
        .reference_snapshot(
            f.owner(),
            &ReferenceSnapshotRequest::new(f.owner(), old, new, origin, destination).unwrap(),
        )
        .unwrap();
    assert!(
        !database.ordinary_commit_pending_for_test(),
        "snapshot stays marker-free"
    );
    let (origin, destination) = f.fences();
    let conflict = ReferenceRekeyRequest::new(
        f.owner(),
        OperationId::from_uuid(uuid::Uuid::from_u128(1)),
        WorkUnitKey::new(
            255,
            10,
            ParticipantId::from_uuid(uuid::Uuid::from_u128(2)),
            WorkUnitRole::GlobalLibrary,
        ),
        snapshot,
        origin,
        destination,
    )
    .unwrap();
    assert!(matches!(
        database.apply_reference_rekey(f.owner(), &conflict),
        Err(LibraryError::ReferenceReplayConflict)
    ));
    assert!(!database.ordinary_commit_pending_for_test());
    database
        .connection
        .execute("UPDATE reference_receipts SET disposition='missing'", [])
        .unwrap();
    assert!(matches!(
        database.apply_reference_rekey(f.owner(), &request),
        Err(LibraryError::ReferenceMalformedReceipt)
    ));
    assert!(!database.ordinary_commit_pending_for_test());
    assert_eq!(f.count("SELECT COUNT(*) FROM reference_receipts"), 1);
}

#[test]
fn reference_wal_pending_bit_excludes_capacity_changed_source_and_ended_source() {
    for fill in [
        "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<8192) INSERT INTO reference_receipts(command_key,payload,payload_identity,disposition,owner_identity) SELECT 'budget-'||x,x'00',zeroblob(32),'missing','00000000-0000-0000-0000-000000000001' FROM n;",
        "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<256) INSERT INTO reference_receipts(command_key,payload,payload_identity,disposition,owner_identity) SELECT 'budget-'||x,zeroblob(65536),zeroblob(32),'missing','00000000-0000-0000-0000-000000000001' FROM n;",
    ] {
        let mut f = ReferenceFixture::new();
        f.seed();
        let request = f.request(1);
        stop_global(&mut f);
        let mut database =
            LibraryDatabase::open_for_profile_guard(f.guard.as_ref().unwrap(), None).unwrap();
        // Synthetic rows exercise the retention ceiling, never physical admission.
        database.connection.execute_batch(fill).unwrap();
        assert!(matches!(
            database.apply_reference_rekey(f.owner(), &request),
            Err(LibraryError::ReferenceReceiptCapacity)
        ));
        assert!(!database.ordinary_commit_pending_for_test());
        database
            .connection
            .execute_batch("DELETE FROM reference_receipts")
            .unwrap();
        let writer = f.origin.as_mut().unwrap();
        let update = crate::sample_sources::SourceWriterRequest::new(
            OperationId::new(),
            ParticipantId::new(),
            crate::sample_sources::SourceCommandId::new(),
            writer.current_revision().unwrap(),
            vec![crate::sample_sources::SourceCurationUpdate::SetRating {
                path: crate::sample_sources::reconciliation::RootRelativePath::try_from_path(
                    "one.wav".into(),
                )
                .unwrap(),
                rating: crate::sample_sources::Rating::KEEP_1,
            }],
        )
        .unwrap();
        writer.commit_curation(update).unwrap();
        assert!(database.apply_reference_rekey(f.owner(), &request).is_err());
        assert!(!database.ordinary_commit_pending_for_test());
        drop(f.origin.take());
        assert!(database.apply_reference_rekey(f.owner(), &request).is_err());
        assert!(!database.ordinary_commit_pending_for_test());
        assert_eq!(f.count("SELECT COUNT(*) FROM reference_receipts"), 0);
    }
}

#[test]
fn reference_wal_source_change_at_confirmed_commit_keeps_marker_and_uncertainty() {
    let mut f = ReferenceFixture::new();
    f.seed();
    let request = f.request(1);
    let bytes = request.proposal_bytes().to_vec();
    stop_global(&mut f);
    let mut database =
        LibraryDatabase::open_for_profile_guard(f.guard.as_ref().unwrap(), None).unwrap();
    let source_path = f
        .guard
        .as_ref()
        .unwrap()
        .profile_root()
        .join("origin")
        .join(crate::sample_sources::db::DB_FILE_NAME);
    let source = Connection::open(&source_path).unwrap();
    let revision: String = source
        .query_row("SELECT value FROM metadata WHERE key='revision'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let changed = (revision.parse::<u64>().unwrap() + 1).to_string();
    // An independent actual source SQLite commit races the confirmed global COMMIT.
    database.connection.commit_hook(Some(move || {
        assert_eq!(
            source
                .execute(
                    "UPDATE metadata SET value=?1 WHERE key='revision'",
                    [&changed]
                )
                .unwrap(),
            1
        );
        false
    }));
    assert!(matches!(
        database.apply_reference_rekey(f.owner(), &request),
        Err(LibraryError::ReferenceCommitUncertain)
    ));
    assert!(database.ordinary_commit_pending_for_test());
    assert_eq!(f.count("SELECT COUNT(*) FROM reference_receipts"), 1);
    assert_eq!(
        f.count("SELECT COUNT(*) FROM harvest_files WHERE source_id='destination'"),
        1
    );
    database.connection.commit_hook(None::<fn() -> bool>);
    database.maintain_after_ordinary_commit(f.guard.as_ref().unwrap());
    // Restore the fixture audit to obtain genuine live fences; this is not a retry grant.
    Connection::open(source_path)
        .unwrap()
        .execute(
            "UPDATE metadata SET value=?1 WHERE key='revision'",
            [revision],
        )
        .unwrap();
    let (origin, destination) = f.fences();
    let restored = ReferenceRekeyRequest::restore(f.owner(), &bytes, origin, destination).unwrap();
    assert!(matches!(
        database
            .apply_reference_rekey(f.owner(), &restored)
            .unwrap(),
        ReferenceRekeyResult::Replayed(_)
    ));
    assert!(!database.ordinary_commit_pending_for_test());
    assert_eq!(f.count("SELECT COUNT(*) FROM reference_receipts"), 1);
}
