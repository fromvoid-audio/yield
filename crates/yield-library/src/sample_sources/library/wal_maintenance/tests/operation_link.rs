//! Actual retained-worker operation-link maintenance; filler rows qualify rejection only.
use super::*;
use crate::sample_sources::library::operation_link_participant::{
    OPERATION_LINK_MAX_RECEIPTS, test_request,
};
use source::{actual_request, actual_source_receipt};

#[cfg(unix)]
mod ownership;
mod source;

fn apply(
    writer: &GlobalLibraryWriter,
    request: OperationLinkRequest,
) -> Result<OperationLinkResult, LibraryError> {
    writer
        .apply_operation_link(request)
        .unwrap()
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
}

#[test]
fn operation_link_wal_actual_source_receipt_commits_once_and_replay_reads_do_not_checkpoint() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    let _profile = PersistenceProfileGuard::named("link-maintenance-source");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let receipt = actual_source_receipt(&guard, source.path());
    let counter = Arc::new(AtomicUsize::new(0));
    let mut writer = GlobalLibraryWriter::start_with_database_hook_for_test(
        &guard,
        pressure_hook(counter.clone(), Fault::None),
        None,
    )
    .unwrap();
    available(&writer);
    let request = actual_request(guard.owner_identity(), &receipt);
    let committed = apply(&writer, request.clone()).unwrap();
    assert!(matches!(committed, OperationLinkResult::Committed(_)));
    assert_eq!(
        committed.receipt().source_revision(),
        receipt.source_revision().revision()
    );
    assert_eq!(
        committed.receipt().source_command_id(),
        receipt.identity().command_id()
    );
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "actual new receipt must trigger PASSIVE"
    );
    let replay = apply(&writer, request).unwrap();
    assert!(matches!(replay, OperationLinkResult::Replayed(_)));
    assert_eq!(replay.receipt(), committed.receipt());
    writer
        .load_source_registry_snapshot()
        .unwrap()
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    assert!(writer.shutdown());
}

#[test]
fn operation_link_wal_pending_bit_excludes_replay_stale_conflict_failure_and_capacity() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    let _profile = PersistenceProfileGuard::named("link-maintenance-markers");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let mut database = LibraryDatabase::open_for_profile_guard(&guard, None).unwrap();
    // Synthetic requests exercise only this store's classification, not source authority.
    let request = test_request(guard.owner_identity(), 1, 1, 7);
    let receipt = database
        .apply_operation_link(guard.owner_identity(), &request)
        .unwrap();
    assert!(database.ordinary_wal_maintenance.committed.get());
    database.maintain_after_ordinary_commit(&guard);
    assert!(!database.ordinary_wal_maintenance.committed.get());
    assert_eq!(
        database
            .apply_operation_link(guard.owner_identity(), &request)
            .unwrap()
            .receipt(),
        receipt.receipt()
    );
    assert!(
        !database.ordinary_wal_maintenance.committed.get(),
        "throttle cannot hide a replay marker"
    );
    assert!(matches!(
        database.apply_operation_link(&uuid::Uuid::new_v4().to_string(), &request,),
        Err(LibraryError::OperationLinkStaleOwner)
    ));
    assert!(!database.ordinary_wal_maintenance.committed.get());
    assert!(matches!(
        database.apply_operation_link(
            guard.owner_identity(),
            &test_request(guard.owner_identity(), 1, 1, 8),
        ),
        Err(LibraryError::OperationLinkReplayConflict)
    ));
    assert!(!database.ordinary_wal_maintenance.committed.get());
    database.connection.execute_batch("CREATE TRIGGER abort_link BEFORE INSERT ON operation_link_receipts BEGIN SELECT RAISE(ABORT, 'fixture'); END;").unwrap();
    let next = test_request(guard.owner_identity(), 2, 1, 7);
    assert!(
        database
            .apply_operation_link(guard.owner_identity(), &next)
            .is_err()
    );
    assert!(!database.ordinary_wal_maintenance.committed.get());
    database
        .connection
        .execute_batch("DROP TRIGGER abort_link;")
        .unwrap();
    database.connection.commit_hook(Some(|| true));
    assert!(
        database
            .apply_operation_link(guard.owner_identity(), &next)
            .is_err()
    );
    assert!(!database.ordinary_wal_maintenance.committed.get());
    database.connection.commit_hook(None::<fn() -> bool>);
    assert_eq!(
        database
            .connection
            .query_row("SELECT count(*) FROM operation_link_receipts", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    database.connection.execute_batch("
        WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<512)
        INSERT INTO operation_link_receipts(link_key,payload,payload_identity,owner_identity)
        SELECT 'budget-' || x, zeroblob(32768), zeroblob(32), '00000000-0000-0000-0000-000000000001' FROM n;").unwrap();
    assert!(matches!(
        database.apply_operation_link(guard.owner_identity(), &next),
        Err(LibraryError::OperationLinkReceiptCapacity)
    ));
    assert!(!database.ordinary_wal_maintenance.committed.get());
    assert_eq!(
        database
            .apply_operation_link(guard.owner_identity(), &request)
            .unwrap()
            .receipt(),
        receipt.receipt()
    );
    assert!(!database.ordinary_wal_maintenance.committed.get());
    database
        .connection
        .execute(
            "DELETE FROM operation_link_receipts WHERE link_key LIKE 'budget-%'",
            [],
        )
        .unwrap();
    database
        .connection
        .execute(
            "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<?1)
        INSERT INTO operation_link_receipts(link_key,payload,payload_identity,owner_identity)
        SELECT 'budget-' || x, x'00', zeroblob(32), '00000000-0000-0000-0000-000000000001' FROM n",
            [OPERATION_LINK_MAX_RECEIPTS - 1],
        )
        .unwrap();
    assert!(matches!(
        database.apply_operation_link(guard.owner_identity(), &next),
        Err(LibraryError::OperationLinkReceiptCapacity)
    ));
    assert!(!database.ordinary_wal_maintenance.committed.get());
    assert_eq!(
        database
            .apply_operation_link(guard.owner_identity(), &request)
            .unwrap()
            .receipt(),
        receipt.receipt()
    );
    database
        .connection
        .execute(
            "UPDATE operation_link_receipts SET payload=x'7b' WHERE sequence=1",
            [],
        )
        .unwrap();
    assert!(matches!(
        database.apply_operation_link(guard.owner_identity(), &request),
        Err(LibraryError::OperationLinkMalformedReceipt)
    ));
    assert!(!database.ordinary_wal_maintenance.committed.get());
}

#[test]
fn operation_link_wal_actual_worker_maintenance_failure_preserves_committed_receipt() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    for (index, fault) in [Fault::CheckpointDenied, Fault::ObservationAfterCommitDenied]
        .into_iter()
        .enumerate()
    {
        let _profile = PersistenceProfileGuard::named(format!("link-maintenance-fault-{index}"));
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let counter = Arc::new(AtomicUsize::new(0));
        let mut writer = GlobalLibraryWriter::start_with_database_hook_for_test(
            &guard,
            pressure_hook(counter.clone(), fault),
            None,
        )
        .unwrap();
        available(&writer);
        let request = test_request(guard.owner_identity(), 1, 1, 7);
        let committed = apply(&writer, request.clone()).unwrap();
        assert!(matches!(committed, OperationLinkResult::Committed(_)));
        assert_eq!(
            apply(&writer, request).unwrap().receipt(),
            committed.receipt()
        );
        assert_eq!(
            counter.load(Ordering::SeqCst),
            if index == 0 { 1 } else { 0 }
        );
        assert!(writer.shutdown());
    }
}

#[test]
fn operation_link_wal_actual_worker_preserves_pinned_reader_snapshot() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    let _profile = PersistenceProfileGuard::named("link-maintenance-reader");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let counter = Arc::new(AtomicUsize::new(0));
    let mut writer = GlobalLibraryWriter::start_with_database_hook_for_test(
        &guard,
        pressure_hook(counter.clone(), Fault::None),
        None,
    )
    .unwrap();
    available(&writer);
    let reader = Connection::open_with_flags(
        guard.profile_root().join(LIBRARY_DB_FILE_NAME),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    reader
        .execute_batch("BEGIN; SELECT count(*) FROM operation_link_receipts;")
        .unwrap();
    apply(&writer, test_request(guard.owner_identity(), 1, 1, 7)).unwrap();
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    assert_eq!(
        reader
            .query_row("SELECT count(*) FROM operation_link_receipts", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    reader.execute_batch("ROLLBACK").unwrap();
    assert_eq!(
        reader
            .query_row("SELECT count(*) FROM operation_link_receipts", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    drop(reader);
    assert!(writer.shutdown());
}

#[test]
fn operation_link_wal_genuine_reacquisition_replays_historical_source_receipt_without_checkpoint() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    let _profile = PersistenceProfileGuard::named("link-maintenance-restart");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let source_receipt = actual_source_receipt(&guard, source.path());
    let first_counter = Arc::new(AtomicUsize::new(0));
    let mut writer = GlobalLibraryWriter::start_with_database_hook_for_test(
        &guard,
        pressure_hook(first_counter.clone(), Fault::None),
        None,
    )
    .unwrap();
    available(&writer);
    let original = actual_request(guard.owner_identity(), &source_receipt);
    let committed = apply(&writer, original.clone()).unwrap();
    assert_eq!(first_counter.load(Ordering::SeqCst), 1);
    assert!(writer.shutdown());
    drop(writer);
    drop(guard);
    let fresh = WritableProfileGuard::acquire_current().unwrap();
    let counter = Arc::new(AtomicUsize::new(0));
    let hook_counter = counter.clone();
    let mut writer = GlobalLibraryWriter::start_with_database_hook_for_test(
        &fresh,
        Arc::new(move |connection: &Connection| {
            connection
                .execute_batch(
                    "PRAGMA wal_autocheckpoint=0;
                INSERT INTO maintenance_payload VALUES (zeroblob(34603008));",
                )
                .unwrap();
            assert!(
                crate::sqlite_wal::retained_wal_bytes(connection).unwrap()
                    >= crate::WAL_SOFT_WATERMARK_BYTES
            );
            let counter = hook_counter.clone();
            connection.authorizer(Some(move |context: AuthContext<'_>| {
                if matches!(
                    context.action,
                    AuthAction::Pragma {
                        pragma_name: "wal_checkpoint",
                        ..
                    }
                ) {
                    counter.fetch_add(1, Ordering::SeqCst);
                }
                Authorization::Allow
            }));
        }),
        None,
    )
    .unwrap();
    available(&writer);
    assert!(matches!(
        apply(&writer, original),
        Err(LibraryError::OperationLinkStaleOwner)
    ));
    let replay = apply(
        &writer,
        actual_request(fresh.owner_identity(), &source_receipt),
    )
    .unwrap();
    assert!(matches!(replay, OperationLinkResult::Replayed(_)));
    assert_eq!(replay.receipt(), committed.receipt());
    assert_ne!(
        replay.receipt().owner_identity().to_string(),
        fresh.owner_identity()
    );
    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "fresh policy under pressure must not checkpoint replay"
    );
    assert!(writer.shutdown());
}
