//! Real authored-owner historical recovery, not synthetic participant completion.
use super::*;
use rusqlite::params;
fn key(request: &HarvestAuthoredRequest) -> HarvestAuthoredReceiptKey {
    HarvestAuthoredReceiptKey::new(request.operation()).unwrap()
}
fn observe(
    f: &Fixture,
    key: HarvestAuthoredReceiptKey,
) -> Result<Option<HarvestAuthoredReceiptObservation>, LibraryError> {
    f.global()
        .observe_harvest_authored_receipt(key)
        .unwrap()
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
}
#[test]
fn authored_historical_lost_reply_preserves_later_edit_and_generation() {
    let f = Fixture::new();
    let request = f.request(HarvestState::Seen);
    let selected = key(&request);
    assert!(observe(&f, selected).unwrap().is_none());
    assert!(HarvestAuthoredReceiptKey::new(OperationId::from_uuid(Uuid::nil())).is_err());
    let proposal = *blake3::hash(request.proposal_bytes()).as_bytes();
    drop(
        f.global()
            .set_harvest_authored_state(request.clone())
            .unwrap(),
    );
    let original = observe(&f, selected).unwrap().unwrap();
    assert_eq!(original.operation(), selected.operation());
    assert_eq!(original.disposition(), HarvestAuthoredDisposition::Applied);
    assert_eq!(original.requested_state(), HarvestState::Seen);
    assert_eq!(
        original.owner_identity().to_string(),
        f.guard().owner_identity()
    );
    assert_eq!(original.proposal_identity(), &proposal);
    f.apply(f.request(HarvestState::Done)).unwrap();
    let revision = f.global().observed_mutation_revision_for_test();
    assert_eq!(observe(&f, selected).unwrap(), Some(original));
    assert_eq!(f.global().observed_mutation_revision_for_test(), revision);
    assert_eq!(f.state(), "done");
    assert!(matches!(
        f.apply(request).unwrap(),
        HarvestAuthoredResult::Replayed(_)
    ));
    assert_eq!(f.state(), "done");
    assert_eq!(f.count(), 2);
    assert!(
        observe(
            &f,
            HarvestAuthoredReceiptKey::new(OperationId::new()).unwrap()
        )
        .unwrap()
        .is_none()
    );
}
#[test]
fn authored_historical_restart_reads_without_source_or_request() {
    let mut f = Fixture::new();
    let request = f.request(HarvestState::Touched);
    let selected = key(&request);
    f.apply(request).unwrap();
    let original = observe(&f, selected).unwrap().unwrap();
    f.source
        .take()
        .unwrap()
        .into_catalog_database()
        .close()
        .unwrap();
    let mut global = f.global.take().unwrap();
    global.shutdown();
    drop(global);
    drop(f.guard.take());
    let guard = WritableProfileGuard::acquire_current().unwrap();
    assert_ne!(
        guard.owner_identity(),
        original.owner_identity().to_string()
    );
    let global = GlobalLibraryWriter::start(&guard).unwrap();
    wait_global(&global);
    f.guard = Some(guard);
    f.global = Some(global);
    assert_eq!(observe(&f, selected).unwrap(), Some(original));
    assert_eq!(f.state(), "touched");
    assert_eq!(f.count(), 1);
}
#[test]
fn authored_historical_inventory_refusal_preserves_exact_replay() {
    let f = Fixture::new();
    let request = f.request(HarvestState::Seen);
    let selected = key(&request);
    f.apply(request.clone()).unwrap();
    f.db().execute_batch("WITH RECURSIVE n(x) AS(SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<8192) INSERT INTO harvest_authored_receipts(command_key,payload,result,identity,owner) SELECT 'capacity-'||x,x'7b7d',x'7b7d',zeroblob(32),'00000000-0000-0000-0000-000000000001' FROM n;").unwrap();
    assert!(matches!(
        observe(&f, selected),
        Err(LibraryError::HarvestAuthoredMalformedReceipt)
    ));
    assert!(matches!(
        f.apply(request).unwrap(),
        HarvestAuthoredResult::Replayed(_)
    ));
    assert_eq!(f.state(), "seen");
    assert_eq!(f.count(), 8193);
}
#[test]
fn harvest_historical_receipt_refuses_corrupt_original_rows_without_mutation() {
    for sql in [
        "PRAGMA ignore_check_constraints=ON;UPDATE harvest_authored_receipts SET result=zeroblob(1000000);PRAGMA ignore_check_constraints=OFF;",
        "UPDATE harvest_authored_receipts SET identity=zeroblob(32)",
        "ALTER TABLE harvest_authored_receipts RENAME TO saved;CREATE TABLE harvest_authored_receipts AS SELECT * FROM saved;INSERT INTO harvest_authored_receipts SELECT * FROM saved;",
    ] {
        let f = Fixture::new();
        let request = f.request(HarvestState::Seen);
        let selected = key(&request);
        f.apply(request).unwrap();
        let c = f.db();
        c.execute_batch(sql).unwrap();
        let before: (i64, i64) = c
            .query_row(
                "SELECT count(*), sum(length(result)) FROM harvest_authored_receipts",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(matches!(
            observe(&f, selected),
            Err(LibraryError::HarvestAuthoredMalformedReceipt)
        ));
        let after: (i64, i64) = c
            .query_row(
                "SELECT count(*), sum(length(result)) FROM harvest_authored_receipts",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(before, after);
        assert_eq!(f.state(), "seen");
    }
}

#[test]
fn harvest_historical_receipt_refuses_rehashed_foreign_scope_and_changed_key() {
    for foreign_profile in [true, false] {
        let f = Fixture::new();
        let request = f.request(HarvestState::Seen);
        let selected = key(&request);
        f.apply(request.clone()).unwrap();
        let c = f.db();
        let (bytes, result, owner): (Vec<u8>, Vec<u8>, String) = c
            .query_row(
                "SELECT payload,result,owner FROM harvest_authored_receipts",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        let mut payload = payload::Payload::decode(&bytes).unwrap();
        if foreign_profile {
            payload.profile = "foreign-profile-object".into();
        } else {
            payload.operation = OperationId::new();
        }
        let changed = payload.encode().unwrap();
        let mut digest = blake3::Hasher::new();
        digest.update(b"yield.harvest-authored.receipt.v1\0");
        digest.update(&(changed.len() as u64).to_le_bytes());
        digest.update(&changed);
        digest.update(&(result.len() as u64).to_le_bytes());
        digest.update(&result);
        digest.update(owner.as_bytes());
        c.execute(
            "UPDATE harvest_authored_receipts SET payload=?1,identity=?2",
            params![changed, digest.finalize().as_bytes().as_slice()],
        )
        .unwrap();
        assert!(matches!(
            observe(&f, selected),
            Err(LibraryError::HarvestAuthoredMalformedReceipt)
        ));
        let unchanged: Vec<u8> = c
            .query_row("SELECT payload FROM harvest_authored_receipts", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(unchanged, changed);
        // The original current request still reports changed canonical intent as conflict.
        // Historical lookup separately refuses the foreign profile/operation as corruption.
        assert!(matches!(
            f.apply(request),
            Err(LibraryError::HarvestAuthoredReplayConflict)
        ));
        assert_eq!(f.state(), "seen");
        assert_eq!(f.count(), 1);
    }
}

#[test]
fn harvest_historical_receipt_bounds_unindexed_foreign_key_before_comparison() {
    let f = Fixture::new();
    let request = f.request(HarvestState::Seen);
    let selected = key(&request);
    f.apply(request.clone()).unwrap();
    let original = observe(&f, selected).unwrap().unwrap();
    let c = f.db();
    c.execute_batch("ALTER TABLE harvest_authored_receipts RENAME TO saved; CREATE TABLE harvest_authored_receipts AS SELECT * FROM saved; INSERT INTO harvest_authored_receipts(sequence,command_key,payload,result,identity,owner) SELECT 1000,CAST(zeroblob(1000000) AS TEXT),payload,result,identity,owner FROM saved;").unwrap();
    let missing = HarvestAuthoredReceiptKey::new(OperationId::new()).unwrap();
    for lookup in [selected, missing] {
        assert!(matches!(
            observe(&f, lookup),
            Err(LibraryError::HarvestAuthoredMalformedReceipt)
        ));
    }
    assert_eq!(f.count(), 2);
    assert_eq!(f.state(), "seen");
    let bytes: i64 = c
        .query_row(
            "SELECT max(length(CAST(command_key AS BLOB))) FROM harvest_authored_receipts",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(bytes, 1000000);
    c.execute(
        "DELETE FROM harvest_authored_receipts WHERE sequence=1000",
        [],
    )
    .unwrap();
    assert_eq!(observe(&f, selected).unwrap(), Some(original));
    assert!(observe(&f, missing).unwrap().is_none());
    assert!(matches!(
        f.apply(request),
        Ok(HarvestAuthoredResult::Replayed(_))
    ));
}

#[test]
fn harvest_historical_receipt_bounds_recursive_schema_work_and_restores_queue() {
    let f = Fixture::new();
    let request = f.request(HarvestState::Seen);
    let selected = key(&request);
    f.apply(request.clone()).unwrap();
    let original = observe(&f, selected).unwrap().unwrap();
    let revision = f.global().observed_mutation_revision_for_test();
    let c = f.db();
    c.execute_batch(
        "ALTER TABLE harvest_authored_receipts RENAME TO saved_history;
         CREATE VIEW harvest_authored_receipts AS
         WITH RECURSIVE n(x) AS (
           VALUES(0) UNION ALL SELECT x+1 FROM n WHERE x<1000000
         )
         SELECT saved_history.* FROM saved_history JOIN n ON n.x=1000000;",
    )
    .unwrap();
    assert!(matches!(
        observe(&f, selected),
        Err(LibraryError::HarvestAuthoredMalformedReceipt)
    ));
    assert_eq!(
        c.query_row("SELECT count(*) FROM saved_history", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(f.state(), "seen");
    assert_eq!(f.global().observed_mutation_revision_for_test(), revision);
    c.execute_batch(
        "DROP VIEW harvest_authored_receipts;
         ALTER TABLE saved_history RENAME TO harvest_authored_receipts;",
    )
    .unwrap();
    assert_eq!(observe(&f, selected).unwrap(), Some(original));
    assert!(matches!(
        f.apply(request).unwrap(),
        HarvestAuthoredResult::Replayed(_)
    ));
    assert_eq!(f.count(), 1);
    assert_eq!(f.count(), 1);
}
