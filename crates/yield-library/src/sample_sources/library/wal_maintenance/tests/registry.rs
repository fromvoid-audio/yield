use super::*;
use crate::operation_journal::work_plan::{WorkUnitKey, WorkUnitRole};
use crate::operation_journal::{OperationId, ParticipantId};
use crate::sample_sources::{SampleSource, SourceId, SourceMetadataStorage, SourceRole};

fn snapshot(root: &std::path::Path, id: &str) -> SourceRegistrySnapshot {
    SourceRegistrySnapshot::new(vec![SampleSource {
        id: SourceId::from_string(id),
        root: root.join(id),
        role: SourceRole::Normal,
        metadata_storage: SourceMetadataStorage::SourceFolder,
        primary_import_folder: crate::sample_sources::default_primary_import_folder(),
    }])
}

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
            ParticipantId::from_uuid(uuid::Uuid::from_u128(2172)),
            WorkUnitRole::GlobalLibrary,
        ),
        expected,
        replacement,
    )
    .unwrap()
}

fn result(
    writer: &GlobalLibraryWriter,
    request: SourceRegistryParticipantRequest,
) -> Result<SourceRegistryParticipantReceipt, LibraryError> {
    writer
        .apply_source_registry_participant(request)
        .unwrap()
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
}

#[test]
fn registry_wal_new_applied_and_rejected_receipts_trigger_actual_worker_maintenance() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    for changed in [false, true] {
        let _profile = PersistenceProfileGuard::named(format!("registry-maintenance-{changed}"));
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let counter = Arc::new(AtomicUsize::new(0));
        let mut writer = GlobalLibraryWriter::start_with_database_hook_for_test(
            &guard,
            pressure_hook(counter.clone(), Fault::None),
            None,
        )
        .unwrap();
        available(&writer);
        let empty = SourceRegistrySnapshot::new(Vec::new());
        let expected = if changed {
            snapshot(temp.path(), "expected")
        } else {
            empty
        };
        let replacement = snapshot(temp.path(), "new");
        let command = request(guard.owner_identity(), 1, &expected, replacement.clone());
        let receipt = result(&writer, command.clone()).unwrap();
        assert_eq!(
            receipt.disposition(),
            if changed {
                SourceRegistryParticipantDisposition::RegistryChanged
            } else {
                SourceRegistryParticipantDisposition::Applied
            }
        );
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "a new receipt is a real commit, including a retained rejection"
        );
        assert_eq!(result(&writer, command).unwrap(), receipt);
        let stored = writer
            .load_source_registry_snapshot()
            .unwrap()
            .recv()
            .unwrap()
            .unwrap();
        assert_eq!(stored.as_slice().len(), usize::from(!changed));
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert!(writer.shutdown());
    }
}

#[test]
fn registry_wal_failures_preserve_committed_receipt_and_registry() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    for (index, fault) in [Fault::CheckpointDenied, Fault::ObservationAfterCommitDenied]
        .into_iter()
        .enumerate()
    {
        let _profile = PersistenceProfileGuard::named(format!("registry-fault-{index}"));
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let counter = Arc::new(AtomicUsize::new(0));
        let mut writer = GlobalLibraryWriter::start_with_database_hook_for_test(
            &guard,
            pressure_hook(counter.clone(), fault),
            None,
        )
        .unwrap();
        available(&writer);
        let replacement = snapshot(temp.path(), "committed");
        let command = request(
            guard.owner_identity(),
            1,
            &SourceRegistrySnapshot::new(Vec::new()),
            replacement.clone(),
        );
        let receipt = result(&writer, command.clone()).unwrap();
        assert_eq!(
            receipt.disposition(),
            SourceRegistryParticipantDisposition::Applied
        );
        assert_eq!(result(&writer, command).unwrap(), receipt);
        let stored = writer
            .load_source_registry_snapshot()
            .unwrap()
            .recv()
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(stored.as_slice()).unwrap(),
            serde_json::to_value(replacement.as_slice()).unwrap()
        );
        assert_eq!(counter.load(Ordering::SeqCst), usize::from(index == 0));
        assert!(writer.shutdown());
    }
}

#[test]
fn registry_wal_marker_excludes_replay_conflict_stale_and_rolled_back_insert() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    let _profile = PersistenceProfileGuard::named("registry-marker");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let mut database = LibraryDatabase::open_for_profile_guard(&guard, None).unwrap();
    let empty = SourceRegistrySnapshot::new(Vec::new());
    let first = snapshot(temp.path(), "first");
    let command = request(guard.owner_identity(), 1, &empty, first.clone());
    let receipt = database
        .apply_source_registry_participant(guard.owner_identity(), &command)
        .unwrap();
    assert!(database.ordinary_wal_maintenance.committed.replace(false));
    assert_eq!(
        database
            .apply_source_registry_participant(guard.owner_identity(), &command)
            .unwrap(),
        receipt
    );
    assert!(!database.ordinary_wal_maintenance.committed.get());
    let conflict = request(
        guard.owner_identity(),
        1,
        &empty,
        snapshot(temp.path(), "conflict"),
    );
    assert!(
        database
            .apply_source_registry_participant(guard.owner_identity(), &conflict)
            .is_err()
    );
    assert!(!database.ordinary_wal_maintenance.committed.get());
    assert!(
        database
            .apply_source_registry_participant(&uuid::Uuid::from_u128(999).to_string(), &command)
            .is_err()
    );
    assert!(!database.ordinary_wal_maintenance.committed.get());
    database
        .connection
        .authorizer(Some(|context: AuthContext<'_>| {
            if matches!(
                context.action,
                AuthAction::Insert {
                    table_name: "source_registry_participant_receipts"
                }
            ) {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }));
    let failed = request(
        guard.owner_identity(),
        2,
        &first,
        snapshot(temp.path(), "failed"),
    );
    assert!(
        database
            .apply_source_registry_participant(guard.owner_identity(), &failed)
            .is_err()
    );
    assert!(!database.ordinary_wal_maintenance.committed.get());
    database
        .connection
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
    assert_eq!(
        serde_json::to_value(database.load_source_registry().unwrap().as_slice()).unwrap(),
        serde_json::to_value(first.as_slice()).unwrap()
    );
    let rejected = request(
        guard.owner_identity(),
        3,
        &empty,
        snapshot(temp.path(), "rejected"),
    );
    assert_eq!(
        database
            .apply_source_registry_participant(guard.owner_identity(), &rejected)
            .unwrap()
            .disposition(),
        SourceRegistryParticipantDisposition::RegistryChanged
    );
    assert!(database.ordinary_wal_maintenance.committed.replace(false));
    // Synthetic full-ledger rows exercise only the bounded receipt budget, not effects authority.
    database.connection.execute("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x < ?1) INSERT INTO source_registry_participant_receipts (operation_id, work_key, payload_identity, owner_identity, disposition) SELECT printf('%08x-0000-0000-0000-000000000000', x), '{}', zeroblob(32), ?2, 'registry_changed' FROM n", rusqlite::params![super::super::super::registry_participant::SOURCE_REGISTRY_PARTICIPANT_MAX_RECEIPTS - 2, guard.owner_identity()]).unwrap();
    let at_capacity = request(
        guard.owner_identity(),
        4,
        &first,
        snapshot(temp.path(), "capacity"),
    );
    assert!(matches!(
        database.apply_source_registry_participant(guard.owner_identity(), &at_capacity),
        Err(LibraryError::SourceRegistryParticipantReceiptCapacity)
    ));
    assert!(!database.ordinary_wal_maintenance.committed.get());
    drop(database);
    let mut reopened = LibraryDatabase::open_for_profile_guard(&guard, None).unwrap();
    assert!(!reopened.ordinary_wal_maintenance.committed.get());
    assert_eq!(
        reopened
            .apply_source_registry_participant(guard.owner_identity(), &command)
            .unwrap(),
        receipt
    );
    assert!(!reopened.ordinary_wal_maintenance.committed.get());
}

#[test]
fn registry_wal_passive_checkpoint_preserves_pinned_reader_snapshot() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    let _profile = PersistenceProfileGuard::named("registry-reader");
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
        .execute_batch("BEGIN; SELECT count(*) FROM source_registry_participant_receipts")
        .unwrap();
    result(
        &writer,
        request(
            guard.owner_identity(),
            1,
            &SourceRegistrySnapshot::new(Vec::new()),
            snapshot(temp.path(), "new"),
        ),
    )
    .unwrap();
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    let count = || {
        reader
            .query_row(
                "SELECT count(*) FROM source_registry_participant_receipts",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
    };
    assert_eq!(count(), 0);
    reader.execute_batch("ROLLBACK").unwrap();
    assert_eq!(count(), 1);
    drop(reader);
    assert!(writer.shutdown());
}
#[cfg(unix)]
#[test]
fn registry_wal_replacement_during_checkpoint_is_not_confirmed() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    let _profile = PersistenceProfileGuard::named("maintenance-during-replacement");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let root = guard.profile_root().to_path_buf();
    let displaced = temp.path().join("displaced-during-checkpoint");
    let counter = Arc::new(AtomicUsize::new(0));
    let setup = pressure_hook(counter.clone(), Fault::None);
    let hook_counter = counter.clone();
    let hook_root = root.clone();
    let hook = Arc::new(move |connection: &Connection| {
        setup(connection);
        let counter = hook_counter.clone();
        let root = hook_root.clone();
        let displaced = displaced.clone();
        connection.authorizer(Some(move |context: AuthContext<'_>| {
            if matches!(
                context.action,
                AuthAction::Pragma {
                    pragma_name: "wal_checkpoint",
                    ..
                }
            ) {
                counter.fetch_add(1, Ordering::SeqCst);
                std::fs::rename(&root, &displaced).unwrap();
                std::fs::create_dir(&root).unwrap();
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }));
    });
    let mut writer =
        GlobalLibraryWriter::start_with_database_hook_for_test(&guard, hook, None).unwrap();
    available(&writer);
    let result = result(
        &writer,
        request(
            guard.owner_identity(),
            1,
            &SourceRegistrySnapshot::new(Vec::new()),
            snapshot(temp.path(), "new"),
        ),
    );
    assert!(
        matches!(result, Err(LibraryError::ProfileOwnershipChanged { .. })),
        "replacement during maintenance cannot be acknowledged as current: {result:?}"
    );
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    closed_after_profile_change(&writer);
    assert!(!writer.shutdown());
    assert!(!root.join(LIBRARY_DB_FILE_NAME).exists());
}
#[cfg(unix)]
#[test]
fn registry_wal_replacement_before_maintenance_skips_checkpoint() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    let _profile = PersistenceProfileGuard::named("maintenance-replacement");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let counter = Arc::new(AtomicUsize::new(0));
    let ready = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let mut writer = GlobalLibraryWriter::start_with_database_hook_for_test(
        &guard,
        pressure_hook(counter.clone(), Fault::None),
        Some((ready.clone(), release.clone())),
    )
    .unwrap();
    available(&writer);
    let command = request(
        guard.owner_identity(),
        1,
        &SourceRegistrySnapshot::new(Vec::new()),
        snapshot(temp.path(), "new"),
    );
    let response = writer.apply_source_registry_participant(command).unwrap();
    ready.wait();
    let root = guard.profile_root().to_path_buf();
    std::fs::rename(&root, temp.path().join("displaced-profile")).unwrap();
    std::fs::create_dir(&root).unwrap();
    release.wait();
    assert!(matches!(
        response.recv_timeout(Duration::from_secs(5)).unwrap(),
        Err(LibraryError::ProfileOwnershipChanged { .. })
    ));
    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "postvalidation fails before maintenance can execute"
    );
    closed_after_profile_change(&writer);
    assert!(!writer.shutdown());
    assert!(!root.join(LIBRARY_DB_FILE_NAME).exists());
}

#[test]
fn registry_wal_reacquired_worker_replay_preserves_historical_receipt_and_later_registry() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    let _profile = PersistenceProfileGuard::named("registry-reacquisition");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let empty = SourceRegistrySnapshot::new(Vec::new());
    let first = snapshot(temp.path(), "first");
    let later = snapshot(temp.path(), "later");
    let mut writer = GlobalLibraryWriter::start(&guard).unwrap();
    available(&writer);
    let receipt = result(
        &writer,
        request(guard.owner_identity(), 1, &empty, first.clone()),
    )
    .unwrap();
    complete(
        &writer,
        GlobalMutationRequest::registry(writer.acquisition_id(), 1, later.clone()).unwrap(),
    )
    .unwrap();
    assert!(writer.shutdown());
    drop(writer);
    drop(guard);
    let guard = WritableProfileGuard::acquire_current().unwrap();
    assert_ne!(receipt.owner_identity().to_string(), guard.owner_identity());
    let counter = Arc::new(AtomicUsize::new(0));
    let hook_counter = counter.clone();
    let hook = Arc::new(move |connection: &Connection| {
        connection.execute_batch("PRAGMA wal_autocheckpoint=0; CREATE TABLE replay_pressure(value BLOB); INSERT INTO replay_pressure VALUES (zeroblob(34603008));").unwrap();
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
    });
    let mut writer =
        GlobalLibraryWriter::start_with_database_hook_for_test(&guard, hook, None).unwrap();
    available(&writer);
    assert_eq!(
        result(&writer, request(guard.owner_identity(), 1, &empty, first)).unwrap(),
        receipt
    );
    let stored = writer
        .load_source_registry_snapshot()
        .unwrap()
        .recv()
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(stored.as_slice()).unwrap(),
        serde_json::to_value(later.as_slice()).unwrap()
    );
    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "reacquisition has a fresh throttle, so it cannot mask a replay trigger"
    );
    assert!(writer.shutdown());
}
