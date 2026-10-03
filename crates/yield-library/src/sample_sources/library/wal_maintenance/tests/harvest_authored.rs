//! Actual retained Source observations and Global SQLite commits; no physical qualification.
use super::*;
use crate::operation_journal::{OperationId, StoredRelativePath};
use crate::sample_sources::{SourceDatabase, SourceId, SourceWriter};
use std::path::Path;

fn authored_pressure_hook(
    counter: Arc<AtomicUsize>,
    fault: Fault,
) -> Arc<dyn Fn(&Connection) + Send + Sync> {
    let setup = pressure_hook(counter, fault);
    Arc::new(move |connection| {
        setup(connection);
        let bytes = crate::sqlite_wal::retained_wal_bytes(connection).unwrap();
        assert!(bytes >= crate::WAL_SOFT_WATERMARK_BYTES);
        assert!(bytes < crate::sqlite_wal::WAL_HARD_WATERMARK_BYTES);
    })
}

fn build_source(root: &Path, guard: &WritableProfileGuard, generation: u64) -> SourceWriter {
    let db = SourceDatabase::open_for_catalog_with_profile_guard(root, guard, Path::new("origin"))
        .unwrap();
    db.upsert_file(Path::new("one.wav"), 10, 1).unwrap();
    let mut batch = db.write_batch().unwrap();
    batch
        .upsert_file_with_hash(Path::new("one.wav"), 10, 1, &"a".repeat(64))
        .unwrap();
    batch
        .set_file_identity(Path::new("one.wav"), Some("object-origin"))
        .unwrap();
    batch.commit().unwrap();
    db.begin_source_directory_truth_generation(generation, 0)
        .unwrap();
    let writer =
        SourceWriter::new_for_scan(db, SourceId::from_string("origin"), generation).unwrap();
    let fence = writer.current_scan_fence().unwrap();
    writer
        .catalog_database()
        .write_batch()
        .unwrap()
        .commit_manifest_audit_with_staged_directory_truth(
            generation,
            fence.revision(),
            10,
            fence.root_identity().clone(),
            None,
        )
        .unwrap();
    writer
}
fn observation(
    source: &SourceWriter,
) -> crate::sample_sources::db::SourceContentIdentityObservation {
    source
        .observe_file_identity(StoredRelativePath::new("one.wav").unwrap())
        .unwrap()
        .into_content_identity()
        .unwrap()
}
fn snapshot_request(
    guard: &WritableProfileGuard,
    source: &SourceWriter,
) -> HarvestAuthoredSnapshotRequest {
    HarvestAuthoredSnapshotRequest::new(guard, observation(source)).unwrap()
}
fn request(
    writer: &GlobalLibraryWriter,
    guard: &WritableProfileGuard,
    source: &SourceWriter,
    state: HarvestState,
) -> HarvestAuthoredRequest {
    let snapshot = writer
        .harvest_authored_snapshot(snapshot_request(guard, source))
        .unwrap()
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    HarvestAuthoredRequest::new(
        guard,
        OperationId::new(),
        snapshot,
        observation(source),
        state,
    )
    .unwrap()
}
fn direct_request(
    db: &mut LibraryDatabase,
    guard: &WritableProfileGuard,
    source: &SourceWriter,
    state: HarvestState,
) -> HarvestAuthoredRequest {
    let snapshot = db
        .harvest_authored_snapshot(guard.owner_identity(), &snapshot_request(guard, source))
        .unwrap();
    HarvestAuthoredRequest::new(
        guard,
        OperationId::new(),
        snapshot,
        observation(source),
        state,
    )
    .unwrap()
}
fn apply(
    writer: &GlobalLibraryWriter,
    request: HarvestAuthoredRequest,
) -> Result<HarvestAuthoredResult, LibraryError> {
    writer
        .set_harvest_authored_state(request)
        .unwrap()
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
}
fn count(db: &Connection) -> i64 {
    db.query_row("SELECT count(*) FROM harvest_authored_receipts", [], |r| {
        r.get(0)
    })
    .unwrap()
}

#[test]
fn harvest_authored_wal_actual_worker_applied_and_retained_rejections_checkpoint() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(temp.path().to_path_buf());
    for disposition in [
        HarvestAuthoredDisposition::Applied,
        HarvestAuthoredDisposition::GraphChanged,
        HarvestAuthoredDisposition::IdentityConflict,
    ] {
        let _profile =
            PersistenceProfileGuard::named(format!("authored-maintenance-{disposition:?}"));
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let root = tempfile::tempdir().unwrap();
        let source = build_source(root.path(), &guard, 1);
        let counter = Arc::new(AtomicUsize::new(0));
        let mut writer = GlobalLibraryWriter::start_with_database_hook_for_test(
            &guard,
            authored_pressure_hook(counter.clone(), Fault::None),
            None,
        )
        .unwrap();
        available(&writer);
        let command = if disposition == HarvestAuthoredDisposition::IdentityConflict {
            let db = Connection::open(guard.profile_root().join(LIBRARY_DB_FILE_NAME)).unwrap();
            db.execute_batch("INSERT INTO harvest_files(source_id,relative_path,file_size,modified_ns,content_hash,harvest_state,discovered_at,note) VALUES ('origin','one.wav',10,1,'different','ignored',7,'preserve');").unwrap();
            request(&writer, &guard, &source, HarvestState::Done)
        } else {
            let command = request(&writer, &guard, &source, HarvestState::Done);
            if disposition == HarvestAuthoredDisposition::GraphChanged {
                let db = Connection::open(guard.profile_root().join(LIBRARY_DB_FILE_NAME)).unwrap();
                db.execute_batch("INSERT INTO harvest_files(source_id,relative_path,file_size,modified_ns,content_hash,harvest_state,discovered_at) VALUES ('origin','one.wav',10,1,'changed','ignored',7)").unwrap();
            }
            command
        };
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "snapshot cannot checkpoint"
        );
        let result = apply(&writer, command.clone()).unwrap();
        assert!(matches!(result, HarvestAuthoredResult::Committed(_)));
        assert_eq!(result.receipt().disposition(), disposition);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        let replay = apply(&writer, command).unwrap();
        assert!(matches!(replay, HarvestAuthoredResult::Replayed(_)));
        assert_eq!(replay.receipt().sequence(), result.receipt().sequence());
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert!(writer.shutdown());
        source.into_catalog_database().close().unwrap();
    }
}

#[test]
fn harvest_authored_wal_marker_excludes_snapshot_replay_abort_capacity_admission_and_stale_source()
{
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(temp.path().to_path_buf());
    let _profile = PersistenceProfileGuard::named("authored-marker");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let root = tempfile::tempdir().unwrap();
    let source = build_source(root.path(), &guard, 1);
    let mut db = LibraryDatabase::open_for_profile_guard(&guard, None).unwrap();
    let first = direct_request(&mut db, &guard, &source, HarvestState::Done);
    assert!(!db.ordinary_wal_maintenance.committed.get());
    db.apply_harvest_authored(guard.owner_identity(), &first)
        .unwrap();
    assert!(db.ordinary_wal_maintenance.committed.replace(false));
    assert!(matches!(
        db.apply_harvest_authored(guard.owner_identity(), &first)
            .unwrap(),
        HarvestAuthoredResult::Replayed(_)
    ));
    assert!(!db.ordinary_wal_maintenance.committed.get());
    let next = direct_request(&mut db, &guard, &source, HarvestState::Ignored);
    assert!(matches!(
        db.apply_harvest_authored(&uuid::Uuid::new_v4().to_string(), &next),
        Err(LibraryError::HarvestAuthoredStaleOwner)
    ));
    assert!(!db.ordinary_wal_maintenance.committed.get());
    db.connection.execute_batch("CREATE TRIGGER abort_authored BEFORE INSERT ON harvest_authored_receipts BEGIN SELECT RAISE(ABORT,'fixture'); END").unwrap();
    assert!(
        db.apply_harvest_authored(guard.owner_identity(), &next)
            .is_err()
    );
    assert!(!db.ordinary_wal_maintenance.committed.get());
    assert_eq!(count(&db.connection), 1);
    db.connection
        .execute_batch("DROP TRIGGER abort_authored")
        .unwrap();
    db.connection.commit_hook(Some(|| true));
    assert!(
        db.apply_harvest_authored(guard.owner_identity(), &next)
            .is_err()
    );
    assert!(!db.ordinary_wal_maintenance.committed.get());
    db.connection.commit_hook(None::<fn() -> bool>);
    assert_eq!(count(&db.connection), 1);
    db.connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE")
        .unwrap();
    assert!(matches!(
        db.apply_harvest_authored(guard.owner_identity(), &next),
        Err(LibraryError::WalAdmission(_))
    ));
    assert!(!db.ordinary_wal_maintenance.committed.get());
    db.connection
        .execute_batch("PRAGMA journal_mode=WAL")
        .unwrap();
    // Synthetic rows test ledger limits only; all accepted requests have real owner observations.
    for fill in [
        "WITH RECURSIVE n(v) AS (SELECT 1 UNION ALL SELECT v+1 FROM n WHERE v<8191) INSERT INTO harvest_authored_receipts(command_key,payload,result,identity,owner) SELECT 'capacity-'||v,x'01',x'01',zeroblob(32),'00000000-0000-0000-0000-000000000001' FROM n",
        "WITH RECURSIVE n(v) AS (SELECT 1 UNION ALL SELECT v+1 FROM n WHERE v<128) INSERT INTO harvest_authored_receipts(command_key,payload,result,identity,owner) SELECT 'capacity-'||v,zeroblob(65536),zeroblob(65536),zeroblob(32),'00000000-0000-0000-0000-000000000001' FROM n",
    ] {
        db.connection.execute_batch(fill).unwrap();
        assert!(matches!(
            db.apply_harvest_authored(guard.owner_identity(), &next),
            Err(LibraryError::HarvestAuthoredReceiptCapacity)
        ));
        assert!(!db.ordinary_wal_maintenance.committed.get());
        db.apply_harvest_authored(guard.owner_identity(), &first)
            .unwrap();
        assert!(!db.ordinary_wal_maintenance.committed.get());
        db.connection
            .execute(
                "DELETE FROM harvest_authored_receipts WHERE command_key LIKE 'capacity-%'",
                [],
            )
            .unwrap();
    }
    let source_db = Connection::open(
        guard
            .profile_root()
            .join("origin")
            .join(crate::sample_sources::db::DB_FILE_NAME),
    )
    .unwrap();
    source_db
        .execute("UPDATE wav_files SET content_hash=?1", ["b".repeat(64)])
        .unwrap();
    assert!(
        db.apply_harvest_authored(guard.owner_identity(), &next)
            .is_err()
    );
    assert!(!db.ordinary_wal_maintenance.committed.get());
    assert_eq!(count(&db.connection), 1);
    drop(first);
    drop(next);
    source.into_catalog_database().close().unwrap();
}

#[test]
fn harvest_authored_wal_actual_worker_failed_maintenance_and_pinned_reader_preserve_commit() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(temp.path().to_path_buf());
    for (index, fault) in [
        Fault::None,
        Fault::CheckpointDenied,
        Fault::ObservationAfterCommitDenied,
    ]
    .into_iter()
    .enumerate()
    {
        let _profile = PersistenceProfileGuard::named(format!("authored-fault-{index}"));
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let root = tempfile::tempdir().unwrap();
        let source = build_source(root.path(), &guard, 1);
        let counter = Arc::new(AtomicUsize::new(0));
        let mut writer = GlobalLibraryWriter::start_with_database_hook_for_test(
            &guard,
            authored_pressure_hook(counter.clone(), fault),
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
            .execute_batch("BEGIN; SELECT count(*) FROM harvest_authored_receipts")
            .unwrap();
        let result = apply(
            &writer,
            request(&writer, &guard, &source, HarvestState::Done),
        )
        .unwrap();
        assert_eq!(
            result.receipt().disposition(),
            HarvestAuthoredDisposition::Applied
        );
        assert_eq!(counter.load(Ordering::SeqCst), usize::from(index != 2));
        assert_eq!(count(&reader), 0);
        reader.execute_batch("ROLLBACK").unwrap();
        assert_eq!(count(&reader), 1);
        drop(reader);
        assert!(writer.shutdown());
        source.into_catalog_database().close().unwrap();
    }
}

#[test]
fn harvest_authored_wal_real_source_loss_at_commit_marks_uncertain_receipt_and_reacquired_replay_is_read_only()
 {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(temp.path().to_path_buf());
    let _profile = PersistenceProfileGuard::named("authored-uncertain");
    let mut guard = WritableProfileGuard::acquire_current().unwrap();
    let root = tempfile::tempdir().unwrap();
    let source = build_source(root.path(), &guard, 1);
    let mut db = LibraryDatabase::open_for_profile_guard(&guard, None).unwrap();
    let command = direct_request(&mut db, &guard, &source, HarvestState::Done);
    let bytes = command.proposal_bytes().to_vec();
    let source_path = guard
        .profile_root()
        .join("origin")
        .join(crate::sample_sources::db::DB_FILE_NAME);
    db.connection.commit_hook(Some(move || {
        let source_db = Connection::open(&source_path).unwrap();
        source_db
            .execute("UPDATE wav_files SET content_hash=?1", ["b".repeat(64)])
            .unwrap();
        false
    }));
    assert!(matches!(
        db.apply_harvest_authored(guard.owner_identity(), &command),
        Err(LibraryError::HarvestAuthoredCommitUncertain)
    ));
    assert_eq!(
        count(&db.connection),
        1,
        "SQL committed despite lost source observation"
    );
    assert!(
        db.ordinary_wal_maintenance.committed.replace(false),
        "must mark before post-commit source validation"
    );
    db.connection.commit_hook(None::<fn() -> bool>);
    let replay = HarvestAuthoredRequest::restore(&guard, &bytes, observation(&source)).unwrap();
    let historical = db
        .apply_harvest_authored(guard.owner_identity(), &replay)
        .unwrap();
    assert!(matches!(historical, HarvestAuthoredResult::Replayed(_)));
    assert!(!db.ordinary_wal_maintenance.committed.get());
    // Later user edit must survive restart and a genuinely new profile acquisition.
    db.connection
        .execute(
            "UPDATE harvest_files SET harvest_state='ignored',note='later',discovered_at=9",
            [],
        )
        .unwrap();
    drop(db);
    drop(command);
    drop(replay);
    source.into_catalog_database().close().unwrap();
    drop(guard);
    guard = WritableProfileGuard::acquire_current().unwrap();
    let source = build_source(root.path(), &guard, 2);
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_hook = counter.clone();
    let hook = Arc::new(move |connection: &Connection| {
        connection.execute_batch("PRAGMA wal_autocheckpoint=0; CREATE TABLE replay_pressure(value BLOB); INSERT INTO replay_pressure VALUES (zeroblob(34603008))").unwrap();
        let counter = counter_hook.clone();
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
    let replay = apply(
        &writer,
        HarvestAuthoredRequest::restore(&guard, &bytes, observation(&source)).unwrap(),
    )
    .unwrap();
    assert!(matches!(replay, HarvestAuthoredResult::Replayed(_)));
    assert_eq!(replay.receipt().sequence(), historical.receipt().sequence());
    assert_eq!(
        replay.receipt().owner_identity(),
        historical.receipt().owner_identity()
    );
    assert_ne!(
        replay.receipt().owner_identity().to_string(),
        guard.owner_identity()
    );
    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "fresh policy cannot hide a false replay marker"
    );
    let reader = Connection::open(guard.profile_root().join(LIBRARY_DB_FILE_NAME)).unwrap();
    assert!(reader.query_row("SELECT harvest_state='ignored' AND note='later' AND discovered_at=9 FROM harvest_files", [], |r| r.get::<_, bool>(0)).unwrap());
    drop(reader);
    assert!(writer.shutdown());
    source.into_catalog_database().close().unwrap();
}

#[cfg(unix)]
#[test]
fn harvest_authored_wal_actual_worker_profile_loss_before_or_during_checkpoint_closes_without_confirmation()
 {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(temp.path().to_path_buf());
    for during in [false, true] {
        let _profile = PersistenceProfileGuard::named(format!("authored-root-loss-{during}"));
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let source_root = tempfile::tempdir().unwrap();
        let source = build_source(source_root.path(), &guard, 1);
        let root = guard.profile_root().to_path_buf();
        let displaced = temp.path().join(format!("displaced-{during}"));
        let counter = Arc::new(AtomicUsize::new(0));
        let setup = authored_pressure_hook(counter.clone(), Fault::None);
        let hook_counter = counter.clone();
        let hook_root = root.clone();
        let hook_displaced = displaced.clone();
        let hook = Arc::new(move |connection: &Connection| {
            setup(connection);
            if during {
                let counter = hook_counter.clone();
                let root = hook_root.clone();
                let displaced = hook_displaced.clone();
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
            }
        });
        let ready = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        // Snapshot commands also reach the gate: prepare against the same real database first.
        let mut database = LibraryDatabase::open_for_profile_guard(&guard, None).unwrap();
        let command = direct_request(&mut database, &guard, &source, HarvestState::Done);
        drop(database);
        let mut writer = GlobalLibraryWriter::start_with_database_hook_for_test(
            &guard,
            hook,
            (!during).then(|| (ready.clone(), release.clone())),
        )
        .unwrap();
        available(&writer);
        let reply = writer.set_harvest_authored_state(command).unwrap();
        if !during {
            ready.wait();
            std::fs::rename(&root, &displaced).unwrap();
            std::fs::create_dir(&root).unwrap();
            release.wait();
        }
        let result = reply.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            matches!(result, Err(LibraryError::ProfileOwnershipChanged { .. })),
            "{result:?}"
        );
        assert_eq!(counter.load(Ordering::SeqCst), usize::from(during));
        closed_after_profile_change(&writer);
        assert!(!writer.shutdown());
        assert!(!root.join(LIBRARY_DB_FILE_NAME).exists());
        let reader = Connection::open_with_flags(
            displaced.join(LIBRARY_DB_FILE_NAME),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        assert_eq!(count(&reader), 1);
        drop(reader);
        // This is genuine ownership loss: close reports the lost profile instead of authorizing writes.
        assert!(source.into_catalog_database().close().is_err());
    }
}

#[test]
fn harvest_authored_wal_actual_worker_source_loss_after_commit_remains_uncertain_and_attempts_maintenance()
 {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(temp.path().to_path_buf());
    let _profile = PersistenceProfileGuard::named("authored-worker-source-loss");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let root = tempfile::tempdir().unwrap();
    let source = build_source(root.path(), &guard, 1);
    let mut database = LibraryDatabase::open_for_profile_guard(&guard, None).unwrap();
    let command = direct_request(&mut database, &guard, &source, HarvestState::Done);
    drop(database);
    let counter = Arc::new(AtomicUsize::new(0));
    let ready = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let mut writer = GlobalLibraryWriter::start_with_database_hook_for_test(
        &guard,
        authored_pressure_hook(counter.clone(), Fault::None),
        Some((ready.clone(), release.clone())),
    )
    .unwrap();
    available(&writer);
    let reply = writer.set_harvest_authored_state(command).unwrap();
    ready.wait();
    let db = Connection::open(guard.profile_root().join(LIBRARY_DB_FILE_NAME)).unwrap();
    assert_eq!(count(&db), 1);
    let source_db = Connection::open(
        guard
            .profile_root()
            .join("origin")
            .join(crate::sample_sources::db::DB_FILE_NAME),
    )
    .unwrap();
    source_db
        .execute("UPDATE wav_files SET content_hash=?1", ["b".repeat(64)])
        .unwrap();
    release.wait();
    assert!(matches!(
        reply.recv_timeout(Duration::from_secs(5)).unwrap(),
        Err(LibraryError::HarvestAuthoredCommitUncertain)
    ));
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    assert_eq!(count(&db), 1);
    drop(source_db);
    drop(db);
    assert!(writer.shutdown());
    source.into_catalog_database().close().unwrap();
}

#[test]
fn harvest_authored_wal_direct_marker_records_both_retained_rejections_without_throttle() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(temp.path().to_path_buf());
    let _profile = PersistenceProfileGuard::named("authored-rejection-markers");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let root = tempfile::tempdir().unwrap();
    let source = build_source(root.path(), &guard, 1);
    let mut db = LibraryDatabase::open_for_profile_guard(&guard, None).unwrap();
    let command = direct_request(&mut db, &guard, &source, HarvestState::Done);
    let later = direct_request(&mut db, &guard, &source, HarvestState::Ignored);
    db.apply_harvest_authored(guard.owner_identity(), &later)
        .unwrap();
    assert!(db.ordinary_wal_maintenance.committed.replace(false));
    let changed = db
        .apply_harvest_authored(guard.owner_identity(), &command)
        .unwrap();
    assert_eq!(
        changed.receipt().disposition(),
        HarvestAuthoredDisposition::GraphChanged
    );
    assert!(db.ordinary_wal_maintenance.committed.replace(false));
    db.connection
        .execute("UPDATE harvest_files SET content_hash=?1", ["b".repeat(64)])
        .unwrap();
    let conflict = direct_request(&mut db, &guard, &source, HarvestState::Done);
    assert!(!db.ordinary_wal_maintenance.committed.get());
    let result = db
        .apply_harvest_authored(guard.owner_identity(), &conflict)
        .unwrap();
    assert_eq!(
        result.receipt().disposition(),
        HarvestAuthoredDisposition::IdentityConflict
    );
    assert!(db.ordinary_wal_maintenance.committed.replace(false));
    assert!(matches!(
        db.apply_harvest_authored(guard.owner_identity(), &conflict)
            .unwrap(),
        HarvestAuthoredResult::Replayed(_)
    ));
    assert!(!db.ordinary_wal_maintenance.committed.get());
    drop(command);
    drop(later);
    drop(conflict);
    source.into_catalog_database().close().unwrap();
}
