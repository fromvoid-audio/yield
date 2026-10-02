// Actual global worker/SQLite tests. Synthetic payloads below qualify storage only.
use super::super::super::operation_link_participant::{test_request, OperationLinkResult, OPERATION_LINK_MAX_RECEIPTS};

fn result(writer: &GlobalLibraryWriter, request: OperationLinkRequest) -> Result<OperationLinkResult, LibraryError> {
    writer.apply_operation_link(request).unwrap().recv_timeout(Duration::from_secs(2)).unwrap()
}
fn connection(guard: &WritableProfileGuard) -> Connection {
    Connection::open(guard.profile_root().join(super::super::super::LIBRARY_DB_FILE_NAME)).unwrap()
}

#[test]
fn operation_link_lost_reply_duplicate_multiple_commands_and_restart() {
    let temp = tempdir().unwrap(); let _runtime = TestRuntimeGuard::acquire();
    let (_base, _profile, guard, mut writer) = start_writer(&temp, "link-restart"); wait_until_available(&writer);
    let command = test_request(guard.owner_identity(), 1, 1, 7);
    drop(writer.apply_operation_link(command.clone()).unwrap());
    let receipt = result(&writer, command.clone()).unwrap();
    assert!(matches!(receipt, OperationLinkResult::Replayed(_)));
    assert_eq!(result(&writer, command.clone()).unwrap(), receipt);
    let second = result(&writer, test_request(guard.owner_identity(), 1, 2, 7)).unwrap();
    assert_ne!(second.receipt().sequence(), receipt.receipt().sequence());
    assert_eq!(connection(&guard).query_row("SELECT COUNT(*) FROM operation_link_receipts", [], |r| r.get::<_, i64>(0)).unwrap(), 2);
    assert!(writer.shutdown()); drop(writer); drop(guard);
    let fresh = WritableProfileGuard::acquire_current().unwrap();
    let mut writer = GlobalLibraryWriter::start(&fresh).unwrap(); wait_until_available(&writer);
    assert!(matches!(result(&writer, command), Err(LibraryError::OperationLinkStaleOwner)));
    let replay = result(&writer, test_request(fresh.owner_identity(), 1, 1, 7)).unwrap();
    assert_eq!(replay, receipt);
    assert_ne!(replay.receipt().owner_identity().to_string(), fresh.owner_identity());
    assert!(writer.shutdown());
}

#[test]
fn operation_link_conflict_abort_and_corruption_fail_closed() {
    let temp = tempdir().unwrap(); let _runtime = TestRuntimeGuard::acquire();
    let (_base, _profile, guard, mut writer) = start_writer(&temp, "link-failures"); wait_until_available(&writer);
    let command = test_request(guard.owner_identity(), 1, 1, 7);
    result(&writer, command.clone()).unwrap();
    assert!(matches!(result(&writer, test_request(guard.owner_identity(), 1, 1, 8)), Err(LibraryError::OperationLinkReplayConflict)));
    let db = connection(&guard);
    db.execute_batch("CREATE TRIGGER abort_link BEFORE INSERT ON operation_link_receipts BEGIN SELECT RAISE(ABORT, 'fixture'); END;").unwrap();
    assert!(result(&writer, test_request(guard.owner_identity(), 2, 1, 7)).is_err());
    assert_eq!(db.query_row("SELECT COUNT(*) FROM operation_link_receipts", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
    db.execute_batch("DROP TRIGGER abort_link").unwrap();
    assert!(matches!(result(&writer, test_request(guard.owner_identity(), 2, 1, 7)).unwrap(), OperationLinkResult::Committed(_)));
    db.execute("UPDATE operation_link_receipts SET payload = x'7b' WHERE sequence = 1", []).unwrap();
    assert!(matches!(result(&writer, command), Err(LibraryError::OperationLinkMalformedReceipt)));
    assert!(writer.shutdown());
}

#[test]
fn operation_link_row_and_byte_budgets_preserve_exact_replay() {
    let temp = tempdir().unwrap(); let _runtime = TestRuntimeGuard::acquire();
    let (_base, _profile, guard, mut writer) = start_writer(&temp, "link-capacity"); wait_until_available(&writer);
    let command = test_request(guard.owner_identity(), 1, 1, 7);
    let receipt = result(&writer, command.clone()).unwrap().receipt().clone();
    let db = connection(&guard);
    // Synthetic budget filler rows prove capacity behavior, not source evidence.
    db.execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<512)
      INSERT INTO operation_link_receipts (link_key,payload,payload_identity,owner_identity)
      SELECT 'budget-' || x, zeroblob(32768), zeroblob(32), '00000000-0000-0000-0000-000000000001' FROM n;").unwrap();
    assert!(matches!(result(&writer, test_request(guard.owner_identity(), 2, 1, 7)), Err(LibraryError::OperationLinkReceiptCapacity)));
    assert_eq!(result(&writer, command.clone()).unwrap().receipt(), &receipt);
    db.execute("DELETE FROM operation_link_receipts WHERE link_key LIKE 'budget-%'", []).unwrap();
    db.execute("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<?1)
      INSERT INTO operation_link_receipts (link_key,payload,payload_identity,owner_identity)
      SELECT 'budget-' || x, x'00', zeroblob(32), '00000000-0000-0000-0000-000000000001' FROM n", [OPERATION_LINK_MAX_RECEIPTS - 1]).unwrap();
    assert!(matches!(result(&writer, test_request(guard.owner_identity(), 2, 1, 7)), Err(LibraryError::OperationLinkReceiptCapacity)));
    assert_eq!(result(&writer, command).unwrap().receipt(), &receipt);
    assert!(writer.shutdown());
}

#[test]
fn operation_link_actual_source_receipt_survives_global_failure_and_readback() {
    use crate::operation_journal::{OperationId, ParticipantId};
    use crate::operation_journal::work_plan::{WorkUnitKey, WorkUnitRole};
    use crate::sample_sources::{SourceCommandId, SourceWriter, SourceWriterRequest, SourceCurationUpdate, SourceDatabase, Rating};
    use crate::sample_sources::reconciliation::RootRelativePath;
    let temp = tempdir().unwrap(); let source = tempdir().unwrap(); let _runtime = TestRuntimeGuard::acquire();
    let (_base, _profile, guard, mut global) = start_writer(&temp, "link-real-source"); wait_until_available(&global);
    let db = SourceDatabase::open_for_catalog_with_profile_guard(source.path(), &guard, Path::new("source-db")).unwrap();
    db.upsert_file(Path::new("one.wav"), 10, 1).unwrap();
    db.begin_source_directory_truth_generation(1, 0).unwrap();
    let mut source = SourceWriter::new_for_scan(db, SourceId::from_string("real-source"), 1).unwrap();
    let root = source.current_scan_fence().unwrap().root_identity().clone();
    source.catalog_database().write_batch().unwrap().commit_manifest_audit_with_staged_directory_truth(1, 1, 10, root, None).unwrap();
    let participant = ParticipantId::from_uuid(uuid::Uuid::from_u128(3));
    let request = SourceWriterRequest::new(OperationId::from_uuid(uuid::Uuid::from_u128(1)), participant, SourceCommandId::from_uuid(uuid::Uuid::from_u128(1)), source.current_revision().unwrap(), vec![SourceCurationUpdate::SetRating { path: RootRelativePath::try_from_path("one.wav".into()).unwrap(), rating: Rating::KEEP_1 }]).unwrap();
    let receipt = source.commit_curation(request.clone()).unwrap();
    let key = WorkUnitKey::new(0, 60, participant, WorkUnitRole::SourceAffect);
    let global_key = WorkUnitKey::new(255, 10, ParticipantId::from_uuid(uuid::Uuid::from_u128(2)), WorkUnitRole::GlobalLibrary);
    let link = OperationLinkRequest::new(guard.owner_identity(), key, global_key, &receipt).unwrap();
    assert!(matches!(OperationLinkRequest::new(guard.owner_identity(), global_key, global_key, &receipt), Err(LibraryError::OperationLinkInput)));
    let conn = connection(&guard);
    conn.execute_batch("CREATE TRIGGER abort_link BEFORE INSERT ON operation_link_receipts BEGIN SELECT RAISE(ABORT, 'fixture'); END;").unwrap();
    assert!(result(&global, link.clone()).is_err());
    assert_eq!(source.read_receipt_observation(request.identity()).unwrap(), Some(receipt.clone()));
    conn.execute_batch("DROP TRIGGER abort_link").unwrap();
    let committed = result(&global, link.clone()).unwrap();
    assert_eq!(committed.receipt().source_revision(), receipt.source_revision().revision());
    assert_eq!(committed.receipt().source_command_id(), receipt.identity().command_id());
    assert_eq!(result(&global, link).unwrap().receipt(), committed.receipt());
    assert_eq!(source.commit_curation(request).unwrap(), receipt);
    assert!(global.shutdown());
}

#[test]
fn operation_link_ambiguous_retained_rows_are_not_arbitrarily_replayed() {
    let temp = tempdir().unwrap(); let _runtime = TestRuntimeGuard::acquire();
    let (_base, _profile, guard, mut writer) = start_writer(&temp, "link-ambiguous"); wait_until_available(&writer);
    let command = test_request(guard.owner_identity(), 1, 1, 7);
    result(&writer, command.clone()).unwrap();
    let db = connection(&guard);
    db.execute_batch("ALTER TABLE operation_link_receipts RENAME TO old_links;
      CREATE TABLE operation_link_receipts AS SELECT * FROM old_links;
      INSERT INTO operation_link_receipts SELECT * FROM old_links; DROP TABLE old_links;").unwrap();
    assert!(matches!(result(&writer, command), Err(LibraryError::OperationLinkMalformedReceipt)));
    assert!(writer.shutdown());
}

#[test]
fn operation_link_reordered_submissions_and_restart_keep_one_result_per_key() {
    let temp = tempdir().unwrap();
    let _runtime = TestRuntimeGuard::acquire();
    let (_base, _profile, guard, mut writer) = start_writer(&temp, "link-reordered");
    wait_until_available(&writer);
    // Command IDs describe correlation identity, not a global execution order. A later
    // source commit may be reported first after response loss; neither link may overwrite
    // the other's historical revision. This is not a saga or source mutation ordering test.
    let older = test_request(guard.owner_identity(), 1, 1, 7);
    let newer = test_request(guard.owner_identity(), 1, 2, 8);
    let newer_reply = writer.apply_operation_link(newer.clone()).unwrap();
    let older_reply = writer.apply_operation_link(older.clone()).unwrap();
    let duplicate_newer = writer.apply_operation_link(newer.clone()).unwrap();
    let duplicate_older = writer.apply_operation_link(older.clone()).unwrap();
    let newer_receipt = newer_reply.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
    let older_receipt = older_reply.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
    for (receiver, expected) in [(duplicate_older, &older_receipt), (duplicate_newer, &newer_receipt)] {
        let replay = receiver.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
        assert!(matches!(replay, OperationLinkResult::Replayed(_)));
        assert_eq!(replay.receipt(), expected.receipt());
    }
    assert_eq!(older_receipt.receipt().operation_id(), newer_receipt.receipt().operation_id());
    assert_eq!(older_receipt.receipt().source_revision(), 7);
    assert_eq!(newer_receipt.receipt().source_revision(), 8);
    assert!(newer_receipt.receipt().sequence() < older_receipt.receipt().sequence());
    assert!(writer.shutdown()); drop(writer); drop(guard);
    let fresh = WritableProfileGuard::acquire_current().unwrap();
    let mut reopened = GlobalLibraryWriter::start(&fresh).unwrap(); wait_until_available(&reopened);
    for (command, revision, expected) in [(1, 7, &older_receipt), (2, 8, &newer_receipt), (1, 7, &older_receipt)] {
        let replay = result(&reopened, test_request(fresh.owner_identity(), 1, command, revision)).unwrap();
        assert!(matches!(replay, OperationLinkResult::Replayed(_)));
        assert_eq!(replay.receipt(), expected.receipt());
    }
    assert_eq!(connection(&fresh).query_row("SELECT COUNT(*) FROM operation_link_receipts", [], |r| r.get::<_, i64>(0)).unwrap(), 2);
    assert!(reopened.shutdown());
}
