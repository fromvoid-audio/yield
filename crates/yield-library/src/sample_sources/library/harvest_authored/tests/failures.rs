use super::*;
#[test]
fn harvest_authored_owner_stale_source_and_owner_reject_without_mutation() {
    let mut f = Fixture::new();
    let r = f.request(HarvestState::Done);
    let mut foreign = r.clone();
    foreign.owner = Uuid::new_v4();
    assert!(matches!(
        f.apply(foreign),
        Err(LibraryError::HarvestAuthoredStaleOwner)
    ));
    f.source_db()
        .execute("UPDATE wav_files SET content_hash=?1", ["b".repeat(64)])
        .unwrap();
    assert!(f.apply(r.clone()).is_err());
    assert_eq!(f.count(), 0);
    f.source
        .take()
        .unwrap()
        .into_catalog_database()
        .close()
        .unwrap();
    assert!(f.apply(r).is_err());
    assert_eq!(f.count(), 0);
}
#[test]
fn harvest_authored_owner_changed_cas_and_identity_conflict_are_retained() {
    let f = Fixture::new();
    let r = f.request(HarvestState::Done);
    f.apply(f.request(HarvestState::Ignored)).unwrap();
    let changed = f.apply(r.clone()).unwrap();
    assert_eq!(
        changed.receipt().disposition(),
        HarvestAuthoredDisposition::GraphChanged
    );
    assert!(matches!(
        f.apply(r).unwrap(),
        HarvestAuthoredResult::Replayed(_)
    ));
    assert_eq!(f.state(), "ignored");
    f.db()
        .execute("UPDATE harvest_files SET content_hash=?1", ["b".repeat(64)])
        .unwrap();
    let conflict = f.apply(f.request(HarvestState::Done)).unwrap();
    assert_eq!(
        conflict.receipt().disposition(),
        HarvestAuthoredDisposition::IdentityConflict
    );
    assert_eq!(f.state(), "ignored");
}
#[test]
fn harvest_authored_owner_transaction_abort_rolls_back_state_and_receipt() {
    let f = Fixture::new();
    let r = f.request(HarvestState::Done);
    f.db().execute_batch("CREATE TRIGGER abort_authored BEFORE INSERT ON harvest_authored_receipts BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
    assert!(f.apply(r.clone()).is_err());
    assert_eq!(f.count(), 0);
    let rows: i64 = f
        .db()
        .query_row("SELECT COUNT(*) FROM harvest_files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0);
    f.db().execute_batch("DROP TRIGGER abort_authored").unwrap();
    assert_eq!(
        f.apply(r).unwrap().receipt().disposition(),
        HarvestAuthoredDisposition::Applied
    );
}
#[test]
fn harvest_authored_owner_changed_intent_and_corrupt_receipt_fail_closed() {
    let f = Fixture::new();
    let r = f.request(HarvestState::Done);
    f.apply(r.clone()).unwrap();
    let mut changed = r.clone();
    changed.payload.state = "ignored".into();
    changed.bytes = changed.payload.encode().unwrap();
    assert!(matches!(
        f.apply(changed),
        Err(LibraryError::HarvestAuthoredReplayConflict)
    ));
    assert_eq!(f.state(), "done");
    f.db()
        .execute(
            "UPDATE harvest_authored_receipts SET identity=zeroblob(32)",
            [],
        )
        .unwrap();
    assert!(matches!(
        f.apply(r),
        Err(LibraryError::HarvestAuthoredMalformedReceipt)
    ));
    assert_eq!(f.count(), 1);
}
#[test]
fn harvest_authored_owner_malformed_and_oversized_graph_are_rejected() {
    let f = Fixture::new();
    f.apply(f.request(HarvestState::Done)).unwrap();
    f.db()
        .execute("UPDATE harvest_files SET harvest_state='unknown'", [])
        .unwrap();
    assert!(matches!(
        f.apply(f.request(HarvestState::Ignored)),
        Err(LibraryError::HarvestAuthoredMalformedGraph)
    ));
    f.db()
        .execute("UPDATE harvest_files SET note=?1", ["x".repeat(4097)])
        .unwrap();
    let source = content(f.source(), "one.wav");
    assert!(
        f.global()
            .harvest_authored_snapshot(
                HarvestAuthoredSnapshotRequest::new(f.guard(), source).unwrap()
            )
            .unwrap()
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .is_err()
    );
    assert_eq!(f.count(), 1);
}
#[test]
fn harvest_authored_owner_capacity_preserves_exact_replay_and_rejects_new_effect() {
    let f = Fixture::new();
    let r = f.request(HarvestState::Done);
    f.apply(r.clone()).unwrap();
    let new = f.request(HarvestState::Ignored);
    f.db().execute_batch("WITH RECURSIVE n(v) AS (SELECT 1 UNION ALL SELECT v+1 FROM n WHERE v<8191) INSERT INTO harvest_authored_receipts(command_key,payload,result,identity,owner) SELECT 'capacity-'||v,x'01',x'01',zeroblob(32),'00000000-0000-0000-0000-000000000001' FROM n;").unwrap();
    assert!(matches!(
        f.apply(new),
        Err(LibraryError::HarvestAuthoredReceiptCapacity)
    ));
    assert!(matches!(
        f.apply(r).unwrap(),
        HarvestAuthoredResult::Replayed(_)
    ));
    assert_eq!(f.state(), "done");
    assert_eq!(f.count(), 8192);
}
#[test]
fn harvest_authored_owner_restore_rejects_unbounded_noncanonical_and_unknown_state() {
    let f = Fixture::new();
    let r = f.request(HarvestState::Done);
    assert!(f.restore(&vec![b'x'; 65537]).is_err());
    let mut bytes = r.proposal_bytes().to_vec();
    bytes.push(b' ');
    assert!(f.restore(&bytes).is_err());
    let mut p = r.payload.clone();
    p.state = "unknown".into();
    assert!(p.encode().is_err());
    p = r.payload.clone();
    p.operation = OperationId::from_uuid(Uuid::nil());
    assert!(p.encode().is_err());
}
#[test]
fn harvest_authored_owner_timestamp_trigger_mismatch_rolls_back() {
    let f = Fixture::new();
    f.apply(f.request(HarvestState::Seen)).unwrap();
    let r = f.request(HarvestState::Done);
    f.db().execute_batch("CREATE TRIGGER change_authored AFTER UPDATE ON harvest_files BEGIN UPDATE harvest_files SET note='unexpected' WHERE source_id=NEW.source_id AND relative_path=NEW.relative_path; END;").unwrap();
    assert!(matches!(
        f.apply(r),
        Err(LibraryError::HarvestAuthoredMalformedGraph)
    ));
    assert_eq!(f.state(), "seen");
    assert_eq!(f.count(), 1);
}
#[test]
fn harvest_authored_owner_byte_capacity_preserves_replay() {
    let f = Fixture::new();
    let r = f.request(HarvestState::Done);
    f.apply(r.clone()).unwrap();
    let new = f.request(HarvestState::Ignored);
    f.db().execute_batch("WITH RECURSIVE n(v) AS (SELECT 1 UNION ALL SELECT v+1 FROM n WHERE v<128) INSERT INTO harvest_authored_receipts(command_key,payload,result,identity,owner) SELECT 'bytes-'||v,zeroblob(65536),zeroblob(65536),zeroblob(32),'00000000-0000-0000-0000-000000000001' FROM n;").unwrap();
    assert!(matches!(
        f.apply(new),
        Err(LibraryError::HarvestAuthoredReceiptCapacity)
    ));
    assert!(matches!(
        f.apply(r).unwrap(),
        HarvestAuthoredResult::Replayed(_)
    ));
    assert_eq!(f.state(), "done");
}
#[test]
fn harvest_authored_owner_actual_foreign_profile_source_is_rejected() {
    let f = Fixture::new();
    let source = content(f.source(), "one.wav");
    let _foreign = PersistenceProfileGuard::named("foreign-harvest-profile");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let mut global = GlobalLibraryWriter::start(&guard).unwrap();
    wait_global(&global);
    let result = global
        .harvest_authored_snapshot(HarvestAuthoredSnapshotRequest::new(&guard, source).unwrap())
        .unwrap()
        .recv_timeout(Duration::from_secs(3))
        .unwrap();
    assert!(matches!(
        result,
        Err(LibraryError::HarvestAuthoredStaleOwner)
    ));
    global.shutdown();
}
#[test]
fn harvest_authored_owner_unknown_wal_rejects_new_effect_but_preserves_replay() {
    let mut f = Fixture::new();
    let r = f.request(HarvestState::Done);
    f.apply(r.clone()).unwrap();
    let new = f.request(HarvestState::Ignored);
    let mut g = f.global.take().unwrap();
    g.shutdown();
    drop(g);
    let mut database = LibraryDatabase::open_for_profile_guard(f.guard(), None).unwrap();
    database
        .connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE")
        .unwrap();
    assert!(matches!(
        database.apply_harvest_authored(f.guard().owner_identity(), &new),
        Err(LibraryError::WalAdmission(_))
    ));
    assert!(matches!(
        database
            .apply_harvest_authored(f.guard().owner_identity(), &r)
            .unwrap(),
        HarvestAuthoredResult::Replayed(_)
    ));
    assert_eq!(f.state(), "done");
    assert_eq!(f.count(), 1);
    database
        .connection
        .execute_batch("PRAGMA journal_mode=WAL")
        .unwrap();
    database
        .apply_harvest_authored(f.guard().owner_identity(), &new)
        .unwrap();
    assert_eq!(f.state(), "ignored");
}
#[test]
fn harvest_authored_owner_ambiguous_and_oversized_receipts_fail_before_replay() {
    let f = Fixture::new();
    let r = f.request(HarvestState::Done);
    f.apply(r.clone()).unwrap();
    let db = f.db();
    // Deliberately corrupt the real owner's table schema to exercise fail-closed reads.
    db.execute_batch("ALTER TABLE harvest_authored_receipts RENAME TO saved_receipts; CREATE TABLE harvest_authored_receipts AS SELECT * FROM saved_receipts;").unwrap();
    for change in [
        "UPDATE harvest_authored_receipts SET payload=zeroblob(65537)",
        "UPDATE harvest_authored_receipts SET result=zeroblob(65537)",
        "UPDATE harvest_authored_receipts SET result='wrong-type'",
        "UPDATE harvest_authored_receipts SET owner='not-an-owner'",
        "INSERT INTO harvest_authored_receipts SELECT * FROM saved_receipts",
        "INSERT INTO harvest_authored_receipts SELECT sequence,'different-key',payload,result,identity,owner FROM saved_receipts",
    ] {
        db.execute_batch(change).unwrap();
        assert!(
            matches!(
                f.apply(r.clone()),
                Err(LibraryError::HarvestAuthoredMalformedReceipt)
            ),
            "{change}"
        );
        assert_eq!(f.state(), "done");
        db.execute_batch("DELETE FROM harvest_authored_receipts; INSERT INTO harvest_authored_receipts SELECT * FROM saved_receipts;").unwrap();
    }
    assert!(matches!(
        f.apply(r).unwrap(),
        HarvestAuthoredResult::Replayed(_)
    ));
}
