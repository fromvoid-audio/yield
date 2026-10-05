//! Actual retained-queue historical receipt consumers, not synthetic completion fixtures.
use super::*;

fn key(request: &HarvestCopyRequest) -> HarvestCopyReceiptKey {
    HarvestCopyReceiptKey::new(request.operation(), request.work_key()).unwrap()
}
fn observe(
    f: &Fixture,
    key: HarvestCopyReceiptKey,
) -> Result<Option<HarvestCopyReceiptObservation>, LibraryError> {
    f.global()
        .observe_harvest_copy_receipt(key)
        .unwrap()
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
}

#[test]
fn harvest_historical_receipt_original_lost_reply_has_exact_nonmutating_lookup() {
    let f = Fixture::new();
    let request = f.request();
    let selected = key(&request);
    assert_eq!(observe(&f, selected).unwrap(), None);
    let proposal = *blake3::hash(request.proposal_bytes()).as_bytes();
    drop(f.global().apply_harvest_copy(request).unwrap());
    let observed = observe(&f, selected).unwrap().unwrap();
    assert_eq!(observed.operation(), selected.operation());
    assert_eq!(observed.work_key(), selected.work_key());
    assert_eq!(observed.disposition(), HarvestCopyDisposition::Applied);
    assert_eq!(
        observed.owner_identity().to_string(),
        f.guard().owner_identity()
    );
    assert_eq!(observed.proposal_identity(), &proposal);
    assert!(observed.edge_id().unwrap() > 0);
    let revision = f.global().observed_mutation_revision_for_test();
    let facts = f
        .global()
        .read_harvest_facts(
            super::super::super::HarvestFactsRequest::new(
                f.guard(),
                vec![content(&f.sources()[0], "one.wav").file().clone()],
                super::super::super::HarvestFactsCancellation::new(),
            )
            .unwrap(),
        )
        .unwrap()
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap();
    f.db()
        .execute("UPDATE harvest_files SET note='later authored note'", [])
        .unwrap();
    assert_eq!(observe(&f, selected).unwrap(), Some(observed));
    assert_eq!(f.global().observed_mutation_revision_for_test(), revision);
    assert!(facts.is_current_global_generation());
    let missing = HarvestCopyReceiptKey::new(OperationId::new(), selected.work_key()).unwrap();
    assert_eq!(observe(&f, missing).unwrap(), None);
    assert_eq!(f.count("harvest_derivations"), 1);
    assert_eq!(f.count("harvest_copy_receipts"), 1);
    let unchanged: i64 = f
        .db()
        .query_row(
            "SELECT count(*) FROM harvest_files WHERE note='later authored note'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(unchanged, 2);
}

#[test]
fn harvest_historical_receipt_restart_requires_only_actual_current_global_owner() {
    let mut f = Fixture::new();
    let request = f.request();
    let selected = key(&request);
    f.apply(request).unwrap();
    let before = observe(&f, selected).unwrap().unwrap();
    f.journal.take().unwrap().shutdown();
    for source in f.sources.take().unwrap() {
        source.into_catalog_database().close().unwrap();
    }
    let mut original = f.global.take().unwrap();
    original.shutdown();
    drop(original);
    drop(f.guard.take());
    let guard = WritableProfileGuard::acquire_current().unwrap();
    assert_ne!(guard.owner_identity(), before.owner_identity().to_string());
    let global = GlobalLibraryWriter::start(&guard).unwrap();
    wait_global(&global);
    f.guard = Some(guard);
    f.global = Some(global);
    assert_eq!(observe(&f, selected).unwrap(), Some(before));
    assert_eq!(f.count("harvest_derivations"), 1);
}

#[test]
fn harvest_historical_receipt_refuses_corrupt_original_rows_without_mutation() {
    for sql in [
        "PRAGMA ignore_check_constraints=ON;UPDATE harvest_copy_receipts SET result=zeroblob(1000000);PRAGMA ignore_check_constraints=OFF;",
        "UPDATE harvest_copy_receipts SET identity=zeroblob(32)",
        "ALTER TABLE harvest_copy_receipts RENAME TO saved;CREATE TABLE harvest_copy_receipts AS SELECT * FROM saved;INSERT INTO harvest_copy_receipts SELECT * FROM saved;",
    ] {
        let f = Fixture::new();
        let request = f.request();
        let selected = key(&request);
        f.apply(request).unwrap();
        let c = f.db();
        c.execute_batch(sql).unwrap();
        let before: (i64, i64) = c
            .query_row(
                "SELECT count(*), sum(length(result)) FROM harvest_copy_receipts",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(matches!(
            observe(&f, selected),
            Err(LibraryError::HarvestCopyMalformedReceipt)
        ));
        let after: (i64, i64) = c
            .query_row(
                "SELECT count(*), sum(length(result)) FROM harvest_copy_receipts",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(before, after);
        assert_eq!(f.count("harvest_derivations"), 1);
    }
}

#[test]
fn harvest_historical_receipt_refuses_rehashed_foreign_scope_and_changed_key() {
    for foreign_profile in [true, false] {
        let f = Fixture::new();
        let request = f.request();
        let selected = key(&request);
        f.apply(request).unwrap();
        let c = f.db();
        let (bytes, result, owner): (Vec<u8>, Vec<u8>, String) = c
            .query_row(
                "SELECT payload,result,owner FROM harvest_copy_receipts",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        let mut payload = payload::Payload::decode(&bytes).unwrap();
        if foreign_profile {
            payload.profile = "foreign-profile-object".into();
        } else {
            payload.work =
                WorkUnitKey::new(255, 65535, ParticipantId::new(), WorkUnitRole::Harvest);
        }
        let changed = payload.encode().unwrap();
        let mut digest = blake3::Hasher::new();
        digest.update(b"yield.harvest-copy.receipt.v1\0");
        digest.update(&(changed.len() as u64).to_le_bytes());
        digest.update(&changed);
        digest.update(&(result.len() as u64).to_le_bytes());
        digest.update(&result);
        digest.update(owner.as_bytes());
        c.execute(
            "UPDATE harvest_copy_receipts SET payload=?1,identity=?2",
            params![changed, digest.finalize().as_bytes().as_slice()],
        )
        .unwrap();
        assert!(matches!(
            observe(&f, selected),
            Err(LibraryError::HarvestCopyMalformedReceipt)
        ));
        let unchanged: Vec<u8> = c
            .query_row("SELECT payload FROM harvest_copy_receipts", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(unchanged, changed);
        assert_eq!(f.count("harvest_derivations"), 1);
    }
}

#[test]
fn harvest_historical_receipt_capped_key_and_inventory_preserve_original_replay() {
    assert!(
        HarvestCopyReceiptKey::new(
            OperationId::from_uuid(Uuid::nil()),
            WorkUnitKey::new(255, 1, ParticipantId::new(), WorkUnitRole::Harvest)
        )
        .is_err()
    );
    assert!(
        HarvestCopyReceiptKey::new(
            OperationId::new(),
            WorkUnitKey::new(255, 1, ParticipantId::new(), WorkUnitRole::GlobalLibrary)
        )
        .is_err()
    );
    let f = Fixture::new();
    let request = f.request();
    let selected = key(&request);
    f.apply(request.clone()).unwrap();
    let wrong = HarvestCopyReceiptKey::new(
        selected.operation(),
        WorkUnitKey::new(
            selected.work_key().action_ordinal(),
            65535,
            selected.work_key().participant_id(),
            WorkUnitRole::Harvest,
        ),
    )
    .unwrap();
    assert_eq!(observe(&f, wrong).unwrap(), None);
    let c = f.db();
    c.execute_batch("WITH RECURSIVE n(x) AS(SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<8192) INSERT INTO harvest_copy_receipts(command_key,payload,result,identity,owner) SELECT 'capacity-'||x,x'7b7d',x'7b7d',zeroblob(32),'00000000-0000-0000-0000-000000000001' FROM n;").unwrap();
    assert!(matches!(
        observe(&f, selected),
        Err(LibraryError::HarvestCopyMalformedReceipt)
    ));
    assert!(matches!(
        f.apply(request),
        Ok(HarvestCopyResult::Replayed(_))
    ));
    assert_eq!(f.count("harvest_derivations"), 1);
}

#[test]
fn harvest_historical_receipt_bounds_unindexed_foreign_key_before_comparison() {
    let f = Fixture::new();
    let request = f.request();
    let selected = key(&request);
    f.apply(request.clone()).unwrap();
    let original = observe(&f, selected).unwrap().unwrap();
    let c = f.db();
    c.execute_batch("ALTER TABLE harvest_copy_receipts RENAME TO saved; CREATE TABLE harvest_copy_receipts AS SELECT * FROM saved; INSERT INTO harvest_copy_receipts(sequence,command_key,payload,result,identity,owner) SELECT 1000,CAST(zeroblob(1000000) AS TEXT),payload,result,identity,owner FROM saved;").unwrap();
    let missing = HarvestCopyReceiptKey::new(OperationId::new(), selected.work_key()).unwrap();
    for lookup in [selected, missing] {
        assert!(matches!(
            observe(&f, lookup),
            Err(LibraryError::HarvestCopyMalformedReceipt)
        ));
    }
    assert_eq!(f.count("harvest_copy_receipts"), 2);
    assert_eq!(f.count("harvest_derivations"), 1);
    let bytes: i64 = c
        .query_row(
            "SELECT max(length(CAST(command_key AS BLOB))) FROM harvest_copy_receipts",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(bytes, 1000000);
    c.execute("DELETE FROM harvest_copy_receipts WHERE sequence=1000", [])
        .unwrap();
    assert_eq!(observe(&f, selected).unwrap(), Some(original));
    assert!(observe(&f, missing).unwrap().is_none());
    assert!(matches!(
        f.apply(request),
        Ok(HarvestCopyResult::Replayed(_))
    ));
}

#[test]
fn harvest_historical_receipt_sqlite_limit_restores_on_refusal_and_unwind() {
    use super::super::historical::HistoricalValueLimit;
    use rusqlite::limits::Limit;
    let c = Connection::open_in_memory().unwrap();
    let original = c.limit(Limit::SQLITE_LIMIT_LENGTH).unwrap();
    {
        let _bounded = HistoricalValueLimit::new(&c).unwrap();
        let value = c.query_row("SELECT CAST(zeroblob(1000000) AS TEXT)", [], |r| {
            r.get::<_, String>(0)
        });
        assert!(
            matches!(value, Err(rusqlite::Error::SqliteFailure(code, _)) if code.extended_code == rusqlite::ffi::SQLITE_TOOBIG)
        );
    }
    assert_eq!(c.limit(Limit::SQLITE_LIMIT_LENGTH).unwrap(), original);
    assert_eq!(
        c.query_row("SELECT zeroblob(1000000)", [], |r| r.get::<_, Vec<u8>>(0))
            .unwrap()
            .len(),
        1000000
    );
    c.set_limit(Limit::SQLITE_LIMIT_LENGTH, 32768).unwrap();
    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _bounded = HistoricalValueLimit::new(&c).unwrap();
        assert_eq!(c.limit(Limit::SQLITE_LIMIT_LENGTH).unwrap(), 32768);
        panic!("owned read unwound");
    }));
    assert!(unwind.is_err());
    assert_eq!(c.limit(Limit::SQLITE_LIMIT_LENGTH).unwrap(), 32768);
}

#[test]
fn harvest_historical_receipt_bounds_recursive_schema_work_and_restores_queue() {
    let f = Fixture::new();
    let request = f.request();
    let selected = key(&request);
    f.apply(request.clone()).unwrap();
    let original = observe(&f, selected).unwrap().unwrap();
    let revision = f.global().observed_mutation_revision_for_test();
    let c = f.db();
    c.execute_batch(
        "ALTER TABLE harvest_copy_receipts RENAME TO saved_history;
         CREATE VIEW harvest_copy_receipts AS
         WITH RECURSIVE n(x) AS (
           VALUES(0) UNION ALL SELECT x+1 FROM n WHERE x<1000000
         )
         SELECT saved_history.* FROM saved_history JOIN n ON n.x=1000000;",
    )
    .unwrap();
    assert!(matches!(
        observe(&f, selected),
        Err(LibraryError::HarvestCopyMalformedReceipt)
    ));
    assert_eq!(f.count("saved_history"), 1);
    assert_eq!(f.count("harvest_derivations"), 1);
    assert_eq!(f.global().observed_mutation_revision_for_test(), revision);
    c.execute_batch(
        "DROP VIEW harvest_copy_receipts;
         ALTER TABLE saved_history RENAME TO harvest_copy_receipts;",
    )
    .unwrap();
    assert_eq!(observe(&f, selected).unwrap(), Some(original));
    assert!(matches!(
        f.apply(request).unwrap(),
        HarvestCopyResult::Replayed(_)
    ));
    assert_eq!(f.count("harvest_copy_receipts"), 1);
    assert_eq!(f.count("harvest_derivations"), 1);
}
