// Actual global-owner/SQLite receipt fixtures. No physical source effect or native qualification.
use super::super::super::registry_participant::{
    SOURCE_REGISTRY_PARTICIPANT_MAX_RECEIPTS, SourceRegistryParticipantDisposition,
};
use crate::operation_journal::work_plan::{WorkUnitKey, WorkUnitRole};
use crate::operation_journal::{OperationId, ParticipantId};

fn request(
    owner: &str,
    operation: u128,
    expected: &SourceRegistrySnapshot,
    replacement: SourceRegistrySnapshot,
) -> SourceRegistryParticipantRequest {
    SourceRegistryParticipantRequest::new(
        owner,
        OperationId::from_uuid(uuid::Uuid::from_u128(operation)),
        WorkUnitKey::new(
            u8::MAX,
            1,
            ParticipantId::from_uuid(uuid::Uuid::from_u128(1642)),
            WorkUnitRole::GlobalLibrary,
        ),
        expected,
        replacement,
    )
    .expect("participant request")
}

fn result(
    writer: &GlobalLibraryWriter,
    request: SourceRegistryParticipantRequest,
) -> Result<SourceRegistryParticipantReceipt, LibraryError> {
    writer
        .apply_source_registry_participant(request)
        .expect("queue")
        .recv_timeout(Duration::from_secs(2))
        .expect("owner response")
}

fn snapshot(writer: &GlobalLibraryWriter) -> SourceRegistrySnapshot {
    writer
        .load_source_registry_snapshot()
        .expect("queue load")
        .recv_timeout(Duration::from_secs(2))
        .expect("load response")
        .expect("snapshot")
}

#[test]
fn registry_participant_lost_response_replays_across_new_profile_acquisition_without_reapplying() {
    let temp = tempdir().expect("base");
    let _runtime = TestRuntimeGuard::acquire();
    let (_base, _profile, guard, mut writer) = start_writer(&temp, "participant-restart");
    wait_until_available(&writer);
    let empty = snapshot(&writer);
    let replacement = SourceRegistrySnapshot::new(vec![source(&temp.path().join("one"), "one")]);
    let command = request(guard.owner_identity(), 1, &empty, replacement.clone());
    // Dropping the observation does not cancel already accepted work. The following FIFO load
    // proves it committed even though the first caller never received the result.
    drop(
        writer
            .apply_source_registry_participant(command.clone())
            .expect("lost reply queue"),
    );
    assert_eq!(snapshot(&writer).as_slice()[0].id.as_str(), "one");
    let receipt = result(&writer, command.clone()).expect("durable replay");
    assert_eq!(receipt.payload_identity(), command.payload_identity());
    assert_eq!(
        receipt.disposition(),
        SourceRegistryParticipantDisposition::Applied
    );
    assert_eq!(
        result(&writer, command.clone()).expect("duplicate"),
        receipt
    );
    let later = SourceRegistrySnapshot::new(vec![source(&temp.path().join("later"), "later")]);
    writer
        .replace_source_registry(later)
        .expect("ordinary update")
        .recv_timeout(Duration::from_secs(2))
        .expect("response")
        .expect("update");
    assert!(writer.shutdown());
    drop(writer);
    drop(guard);
    let fresh_guard = WritableProfileGuard::acquire_current().expect("new acquisition");
    assert_ne!(
        uuid::Uuid::parse_str(fresh_guard.owner_identity()).expect("new identity"),
        receipt.owner_identity()
    );
    let mut reopened = GlobalLibraryWriter::start(&fresh_guard).expect("restart");
    wait_until_available(&reopened);
    assert!(matches!(
        result(&reopened, command),
        Err(LibraryError::SourceRegistryParticipantStaleOwner)
    ));
    let rebound = request(fresh_guard.owner_identity(), 1, &empty, replacement);
    assert_eq!(result(&reopened, rebound).expect("restart replay"), receipt);
    assert_eq!(snapshot(&reopened).as_slice()[0].id.as_str(), "later");
    assert!(reopened.shutdown());
}

#[test]
fn registry_participant_conflict_and_stale_snapshot_have_one_retained_disposition() {
    let temp = tempdir().expect("base");
    let _runtime = TestRuntimeGuard::acquire();
    let (_base, _profile, guard, mut writer) = start_writer(&temp, "participant-conflict");
    wait_until_available(&writer);
    let empty = snapshot(&writer);
    let replacement = SourceRegistrySnapshot::new(vec![source(&temp.path().join("one"), "one")]);
    let receipt = result(
        &writer,
        request(guard.owner_identity(), 1, &empty, replacement.clone()),
    )
    .expect("apply");
    assert!(matches!(
        result(
            &writer,
            request(
                guard.owner_identity(),
                1,
                &empty,
                SourceRegistrySnapshot::new(Vec::new())
            )
        ),
        Err(LibraryError::SourceRegistryParticipantReplayConflict)
    ));
    let different_key = WorkUnitKey::new(
        u8::MAX,
        2,
        receipt.key().participant_id(),
        WorkUnitRole::GlobalLibrary,
    );
    let conflict = SourceRegistryParticipantRequest::new(
        guard.owner_identity(),
        receipt.operation_id(),
        different_key,
        &empty,
        replacement.clone(),
    )
    .expect("different work-unit request");
    assert!(matches!(
        result(&writer, conflict),
        Err(LibraryError::SourceRegistryParticipantReplayConflict)
    ));
    let rejected_command = request(guard.owner_identity(), 2, &empty, replacement.clone());
    let rejected = result(&writer, rejected_command.clone()).expect("retained rejection");
    assert_eq!(
        rejected.disposition(),
        SourceRegistryParticipantDisposition::RegistryChanged
    );
    assert!(rejected.sequence() > receipt.sequence());
    writer
        .replace_source_registry(SourceRegistrySnapshot::new(Vec::new()))
        .expect("clear")
        .recv_timeout(Duration::from_secs(2))
        .expect("response")
        .expect("clear result");
    // Even when the expected content later matches again, the original rejected operation is
    // immutable; only a new durable operation may attempt this replacement.
    assert_eq!(
        result(&writer, rejected_command).expect("rejection replay"),
        rejected
    );
    assert!(snapshot(&writer).is_empty());
    assert!(writer.shutdown());
    let mut reopened = GlobalLibraryWriter::start(&guard).expect("restart");
    wait_until_available(&reopened);
    let rejected_command = request(guard.owner_identity(), 2, &empty, replacement);
    assert_eq!(
        result(&reopened, rejected_command).expect("rejected restart"),
        rejected
    );
    assert!(snapshot(&reopened).is_empty());
    assert!(reopened.shutdown());
}

#[test]
fn registry_participant_receipt_insert_failure_rolls_back_registry_and_retained_sources() {
    let temp = tempdir().expect("base");
    let _runtime = TestRuntimeGuard::acquire();
    let (_base, _profile, guard, mut writer) = start_writer(&temp, "participant-rollback");
    wait_until_available(&writer);
    assert!(writer.shutdown());
    let mut db =
        LibraryDatabase::open_for_profile_guard(&guard, None).expect("owned test database");
    let empty = db.load_source_registry().expect("initial");
    let command = request(
        guard.owner_identity(),
        1,
        &empty,
        SourceRegistrySnapshot::new(vec![source(&temp.path().join("one"), "one")]),
    );
    db.connection.execute_batch("CREATE TEMP TRIGGER fail_participant_receipt BEFORE INSERT
        ON source_registry_participant_receipts BEGIN SELECT RAISE(ABORT, 'injected failure'); END;").expect("failure boundary");
    assert!(matches!(
        db.apply_source_registry_participant(guard.owner_identity(), &command),
        Err(LibraryError::Sql(_))
    ));
    assert!(
        db.load_source_registry()
            .expect("registry after rollback")
            .is_empty()
    );
    assert!(
        db.load_retained_sources()
            .expect("retained after rollback")
            .is_empty()
    );
    let count: i64 = db
        .connection
        .query_row(
            "SELECT COUNT(*) FROM source_registry_participant_receipts",
            [],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(count, 0);
    db.connection
        .execute_batch("DROP TRIGGER fail_participant_receipt")
        .expect("remove fault");
    assert_eq!(
        db.apply_source_registry_participant(guard.owner_identity(), &command)
            .expect("safe retry")
            .disposition(),
        SourceRegistryParticipantDisposition::Applied
    );
}

#[test]
fn registry_participant_capacity_preserves_duplicates_and_blocks_new_effects() {
    let temp = tempdir().expect("base");
    let _runtime = TestRuntimeGuard::acquire();
    let (_base, _profile, guard, mut writer) = start_writer(&temp, "participant-capacity");
    wait_until_available(&writer);
    assert!(writer.shutdown());
    let mut db =
        LibraryDatabase::open_for_profile_guard(&guard, None).expect("owned test database");
    let empty = db.load_source_registry().expect("initial");
    let command = request(
        guard.owner_identity(),
        1,
        &empty,
        SourceRegistrySnapshot::new(Vec::new()),
    );
    let receipt = db
        .apply_source_registry_participant(guard.owner_identity(), &command)
        .expect("initial receipt");
    // Ledger-budget fixture only: these synthetic rows cannot qualify physical/source effects.
    db.connection.execute("WITH RECURSIVE n(x) AS (SELECT 2 UNION ALL SELECT x+1 FROM n WHERE x < ?1)
        INSERT INTO source_registry_participant_receipts (operation_id, work_key, payload_identity, owner_identity, disposition)
        SELECT printf('%08x-0000-0000-0000-000000000000', x), '{}', zeroblob(32), ?2, 'registry_changed' FROM n",
        rusqlite::params![SOURCE_REGISTRY_PARTICIPANT_MAX_RECEIPTS, guard.owner_identity()]).expect("bounded full ledger");
    assert_eq!(
        db.apply_source_registry_participant(guard.owner_identity(), &command)
            .expect("replay at capacity"),
        receipt
    );
    let next = request(
        guard.owner_identity(),
        9,
        &empty,
        SourceRegistrySnapshot::new(vec![source(&temp.path().join("one"), "one")]),
    );
    assert!(matches!(
        db.apply_source_registry_participant(guard.owner_identity(), &next),
        Err(LibraryError::SourceRegistryParticipantReceiptCapacity)
    ));
    assert!(db.load_source_registry().expect("unchanged").is_empty());
}

#[test]
fn registry_participant_rejects_unbound_and_oversized_inputs_before_queueing() {
    let empty = SourceRegistrySnapshot::new(Vec::new());
    let operation = OperationId::from_uuid(uuid::Uuid::from_u128(1));
    let key = WorkUnitKey::new(
        0,
        1,
        ParticipantId::from_uuid(uuid::Uuid::from_u128(2)),
        WorkUnitRole::Harvest,
    );
    assert!(matches!(
        SourceRegistryParticipantRequest::new(
            "not-an-owner",
            operation,
            key,
            &empty,
            empty.clone()
        ),
        Err(LibraryError::SourceRegistryParticipantInput { .. })
    ));
    let owner = uuid::Uuid::from_u128(3).to_string();
    assert!(matches!(
        SourceRegistryParticipantRequest::new(&owner, operation, key, &empty, empty.clone()),
        Err(LibraryError::SourceRegistryParticipantInput { .. })
    ));
    let key = WorkUnitKey::new(0, 1, key.participant_id(), WorkUnitRole::GlobalLibrary);
    let oversized = SourceRegistrySnapshot::new(vec![
        source(Path::new("one"), "one");
        SOURCE_REGISTRY_MAX_DESCRIPTORS + 1
    ]);
    assert!(matches!(
        SourceRegistryParticipantRequest::new(&owner, operation, key, &empty, oversized),
        Err(LibraryError::SourceRegistryTooManyDescriptors { .. })
    ));
}

#[test]
fn registry_participant_malformed_retained_receipt_fails_closed_without_reapplying() {
    let temp = tempdir().expect("base");
    let _runtime = TestRuntimeGuard::acquire();
    let (_base, _profile, guard, mut writer) = start_writer(&temp, "participant-corrupt");
    wait_until_available(&writer);
    assert!(writer.shutdown());
    let mut db =
        LibraryDatabase::open_for_profile_guard(&guard, None).expect("owned test database");
    let empty = db.load_source_registry().expect("initial");
    let command = request(
        guard.owner_identity(),
        1,
        &empty,
        SourceRegistrySnapshot::new(vec![source(&temp.path().join("one"), "one")]),
    );
    db.apply_source_registry_participant(guard.owner_identity(), &command)
        .expect("apply");
    db.replace_source_registry(&SourceRegistrySnapshot::new(Vec::new()))
        .expect("later edit");
    // Simulate retained corruption, not a valid production receipt.
    db.connection
        .execute_batch(
            "PRAGMA ignore_check_constraints=ON;
        UPDATE source_registry_participant_receipts SET payload_identity=zeroblob(33);
        PRAGMA ignore_check_constraints=OFF;",
        )
        .expect("corrupt fixture");
    assert!(matches!(
        db.apply_source_registry_participant(guard.owner_identity(), &command),
        Err(LibraryError::SourceRegistryParticipantInput { .. })
    ));
    assert!(
        db.load_source_registry()
            .expect("later edit preserved")
            .is_empty()
    );
}

#[test]
fn registry_participant_ambiguous_retained_relation_preserves_later_registry_edits() {
    let temp = tempdir().expect("base");
    let _runtime = TestRuntimeGuard::acquire();
    let (_base, _profile, guard, mut writer) = start_writer(&temp, "participant-ambiguous");
    wait_until_available(&writer);
    assert!(writer.shutdown());
    let mut db =
        LibraryDatabase::open_for_profile_guard(&guard, None).expect("owned test database");
    let empty = db.load_source_registry().expect("initial");
    let command = request(
        guard.owner_identity(),
        1,
        &empty,
        SourceRegistrySnapshot::new(vec![source(&temp.path().join("one"), "one")]),
    );
    db.apply_source_registry_participant(guard.owner_identity(), &command)
        .expect("apply");
    db.replace_source_registry(&SourceRegistrySnapshot::new(Vec::new()))
        .expect("later edit");
    // Deliberately malformed retained schema: uniqueness is absent and both candidates exist.
    db.connection
        .execute_batch(
            "ALTER TABLE source_registry_participant_receipts RENAME TO original_receipts;
        CREATE TABLE source_registry_participant_receipts AS SELECT * FROM original_receipts;
        INSERT INTO source_registry_participant_receipts SELECT * FROM original_receipts;",
        )
        .expect("ambiguous fixture");
    assert!(matches!(
        db.apply_source_registry_participant(guard.owner_identity(), &command),
        Err(LibraryError::SourceRegistryParticipantReplayConflict)
    ));
    assert!(
        db.load_source_registry()
            .expect("later edit preserved")
            .is_empty()
    );
}

#[test]
fn registry_participant_configured_membership_lifecycle_replays_without_resurrection() {
    let temp = tempdir().expect("base");
    let _runtime = TestRuntimeGuard::acquire();
    let (_base, _profile, guard, mut writer) = start_writer(&temp, "participant-membership");
    wait_until_available(&writer);
    let root = temp.path().join("configured-source");
    let empty = snapshot(&writer);
    let original = source(&root, "stable-configured-id");
    let mut updated = original.clone();
    updated.role = SourceRole::Primary;
    updated.primary_import_folder = PathBuf::from("incoming");
    let original_snapshot = SourceRegistrySnapshot::new(vec![original]);
    let updated_snapshot = SourceRegistrySnapshot::new(vec![updated]);
    let create = request(
        guard.owner_identity(),
        10,
        &empty,
        original_snapshot.clone(),
    );
    let update = request(
        guard.owner_identity(),
        11,
        &original_snapshot,
        updated_snapshot.clone(),
    );
    let remove = request(guard.owner_identity(), 12, &updated_snapshot, empty.clone());
    let mut receipts = Vec::new();
    for command in [&create, &update, &remove] {
        let receipt = result(&writer, command.clone()).expect("configured membership command");
        assert_eq!(
            receipt.disposition(),
            SourceRegistryParticipantDisposition::Applied
        );
        assert_eq!(receipt.payload_identity(), command.payload_identity());
        receipts.push(receipt);
        let active = snapshot(&writer);
        match receipts.len() {
            1 => {
                assert_eq!(active.as_slice()[0].id.as_str(), "stable-configured-id");
                assert_eq!(active.as_slice()[0].role, SourceRole::Normal);
            }
            2 => {
                assert_eq!(active.as_slice()[0].id.as_str(), "stable-configured-id");
                assert_eq!(active.as_slice()[0].role, SourceRole::Primary);
                assert_eq!(
                    active.as_slice()[0].primary_import_folder,
                    PathBuf::from("incoming")
                );
            }
            3 => assert!(active.is_empty()),
            _ => unreachable!(),
        }
    }
    assert!(
        receipts
            .windows(2)
            .all(|pair| pair[0].sequence() < pair[1].sequence())
    );
    assert!(
        receipts
            .windows(2)
            .all(|pair| pair[0].operation_id() != pair[1].operation_id())
    );
    assert!(writer.shutdown());
    let mut reopened = GlobalLibraryWriter::start(&guard).expect("reopen");
    wait_until_available(&reopened);
    // Reordered old completions must not restore an entry removed by the later operation.
    for index in [1, 0, 2, 0] {
        let command = [&create, &update, &remove][index].clone();
        assert_eq!(
            result(&reopened, command).expect("historical replay"),
            receipts[index]
        );
        assert!(snapshot(&reopened).is_empty());
    }
    let retained = reopened
        .lookup_retained_source_for_root(root)
        .expect("retained lookup")
        .recv_timeout(Duration::from_secs(2))
        .expect("lookup response")
        .expect("lookup")
        .expect("retained descriptor");
    assert_eq!(retained.id.as_str(), "stable-configured-id");
    assert_eq!(retained.role, SourceRole::Primary);
    assert_eq!(retained.primary_import_folder, PathBuf::from("incoming"));
    assert!(reopened.shutdown());
    // These are configured metadata outcomes only. No physical source, source-database
    // retirement, origin absence, lifecycle generation, or live lease proof is manufactured.
}

#[test]
fn registry_participant_reversed_replies_preserve_applied_and_rejected_operation_identity() {
    let temp = tempdir().unwrap();
    let _runtime = TestRuntimeGuard::acquire();
    let (_base, _profile, guard, mut writer) = start_writer(&temp, "registry-reversed-replies");
    wait_until_available(&writer);
    let empty = snapshot(&writer);
    let first = request(
        guard.owner_identity(),
        183601,
        &empty,
        SourceRegistrySnapshot::new(vec![source(&temp.path().join("first"), "first")]),
    );
    let second = request(
        guard.owner_identity(),
        183602,
        &empty,
        SourceRegistrySnapshot::new(vec![source(&temp.path().join("second"), "second")]),
    );
    let first_rx = writer
        .apply_source_registry_participant(first.clone())
        .unwrap();
    let second_rx = writer
        .apply_source_registry_participant(second.clone())
        .unwrap();
    // Observation order cannot relabel a committed result or resurrect a stale proposal.
    let rejected = second_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .unwrap();
    let applied = first_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .unwrap();
    assert_eq!(
        applied.operation_id(),
        OperationId::from_uuid(uuid::Uuid::from_u128(183601))
    );
    assert_eq!(
        rejected.operation_id(),
        OperationId::from_uuid(uuid::Uuid::from_u128(183602))
    );
    assert_eq!(applied.payload_identity(), first.payload_identity());
    assert_eq!(rejected.payload_identity(), second.payload_identity());
    assert_eq!(
        applied.disposition(),
        SourceRegistryParticipantDisposition::Applied
    );
    assert_eq!(
        rejected.disposition(),
        SourceRegistryParticipantDisposition::RegistryChanged
    );
    assert_eq!(result(&writer, second.clone()).unwrap(), rejected);
    assert_eq!(result(&writer, first.clone()).unwrap(), applied);
    assert_eq!(snapshot(&writer).as_slice()[0].id.as_str(), "first");
    assert!(writer.shutdown());
    drop(writer);
    drop(guard);
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let mut reopened = GlobalLibraryWriter::start(&guard).unwrap();
    wait_until_available(&reopened);
    let first = request(
        guard.owner_identity(),
        183601,
        &empty,
        SourceRegistrySnapshot::new(vec![source(&temp.path().join("first"), "first")]),
    );
    let second = request(
        guard.owner_identity(),
        183602,
        &empty,
        SourceRegistrySnapshot::new(vec![source(&temp.path().join("second"), "second")]),
    );
    assert_eq!(result(&reopened, second).unwrap(), rejected);
    assert_eq!(result(&reopened, first).unwrap(), applied);
    assert_eq!(snapshot(&reopened).as_slice()[0].id.as_str(), "first");
    assert!(reopened.shutdown());
}
