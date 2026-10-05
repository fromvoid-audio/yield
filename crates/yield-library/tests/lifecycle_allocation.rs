//! Actual normal-build Linux owner observations. No physical reserve or effect grant fixtures.
#![cfg(all(target_os = "linux", not(feature = "test-support")))]
use rusqlite::{Connection, OpenFlags};
use std::{
    fs,
    path::Path,
    thread,
    time::{Duration, Instant},
};
use yield_library::{
    app_dirs::{AppRootGuard, WritableProfileGuard},
    lifecycle_allocation::{LifecycleAllocationError, LifecycleAllocationRole},
    sample_sources::{
        DB_FILE_NAME, GlobalLibraryWriter, GlobalLibraryWriterStatus, SourceDatabase, SourceId,
        SourceRegistryAuditObservation, SourceWriter,
        library::{GlobalPreparationError, GlobalPreparationRequest, LIBRARY_DB_FILE_NAME},
    },
};
fn source(media: &Path, profile: &WritableProfileGuard) -> SourceWriter {
    SourceWriter::new_for_scan(
        SourceDatabase::open_for_catalog_with_profile_guard(media, profile, Path::new("source"))
            .unwrap(),
        SourceId::from_string("source"),
        1,
    )
    .unwrap()
}
fn global(profile: &WritableProfileGuard) -> GlobalLibraryWriter {
    let writer = GlobalLibraryWriter::start(profile).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while writer.status() != GlobalLibraryWriterStatus::Available {
        assert!(Instant::now() < deadline, "{:?}", writer.status());
        thread::sleep(Duration::from_millis(5));
    }
    writer
}
fn request(writer: &GlobalLibraryWriter) -> GlobalPreparationRequest {
    GlobalPreparationRequest::new(
        writer.preparation_owner(),
        [44; 32],
        Instant::now() + Duration::from_secs(2),
        2,
        4096,
    )
    .unwrap()
    .with_lifecycle_allocation()
}
fn read(path: &Path) -> Connection {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
}
fn count(connection: &Connection, table: &str) -> u64 {
    connection
        .query_row(
            &format!("SELECT count(*) FROM (SELECT 1 FROM {table} LIMIT 2)"),
            [],
            |r| r.get(0),
        )
        .unwrap()
}
#[test]
fn actual_pre_audit_source_and_reserved_global_capture_distinct_same_volume_facts_without_mutation()
{
    let temp = tempfile::tempdir().unwrap();
    let media = temp.path().join("media");
    fs::create_dir(&media).unwrap();
    let _root = AppRootGuard::set(temp.path().join("profile")).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let owner = source(&media, &profile);
    let before = owner.current_scan_fence().unwrap();
    let lease_before: (String, u64, i64) =
        read(&profile.profile_root().join("source").join(DB_FILE_NAME))
            .query_row(
                "SELECT owner_identity,lease_epoch,expires_at FROM source_lease",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
    let source_facts = owner
        .capture_lease_maintenance_allocation(Instant::now() + Duration::from_secs(2))
        .unwrap();
    owner
        .validate_lease_maintenance_allocation(&source_facts)
        .unwrap();
    assert_eq!(
        source_facts.subject().audit(),
        &SourceRegistryAuditObservation::Unavailable {}
    );
    assert!(
        owner.current_revision().is_err(),
        "no authoritative audit was fabricated"
    );
    assert_eq!(
        source_facts.allocation().role(),
        LifecycleAllocationRole::SourceLeaseMaintenance
    );
    assert_eq!(
        source_facts.allocation().selected_maximum_pages() * source_facts.allocation().page_size(),
        64 * 1024 * 1024
    );
    assert_eq!(owner.current_scan_fence().unwrap(), before);
    let lease_after = read(&profile.profile_root().join("source").join(DB_FILE_NAME))
        .query_row(
            "SELECT owner_identity,lease_epoch,expires_at FROM source_lease",
            [],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, u64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        lease_after, lease_before,
        "read-only observation never renews lease"
    );
    let mut writer = global(&profile);
    let reservation = writer
        .submit_preparation(request(&writer))
        .unwrap()
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap();
    let observation = reservation.observation();
    let facts = observation.lifecycle_allocation().unwrap();
    assert_eq!(facts.role(), LifecycleAllocationRole::GlobalReferences);
    assert_eq!(facts.main().identity(), observation.database_identity());
    assert_ne!(
        facts.main().identity(),
        source_facts.allocation().main().identity()
    );
    assert_eq!(
        facts.main().device(),
        source_facts.allocation().main().device()
    );
    assert_eq!(
        facts.selected_maximum_pages() * facts.page_size(),
        512 * 1024 * 1024
    );
    for object in [facts.main(), facts.wal(), facts.shm()] {
        assert!(object.write_ceiling() > 0);
        assert_eq!(object.write_ceiling() % object.allocation_unit(), 0);
    }
    let connection = read(&profile.profile_root().join(LIBRARY_DB_FILE_NAME));
    assert_eq!(count(&connection, "source_lifecycle_references"), 0);
    assert_eq!(count(&connection, "source_lifecycle_reference_receipts"), 0);
    drop(connection);
    reservation
        .abort()
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap();
    assert!(writer.shutdown());
    owner.close().unwrap();
}
#[test]
fn actual_source_observations_reject_original_deadline_and_reacquired_writer() {
    let temp = tempfile::tempdir().unwrap();
    let media = temp.path().join("media");
    fs::create_dir(&media).unwrap();
    let _root = AppRootGuard::set(temp.path().join("profile")).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let owner = source(&media, &profile);
    assert!(matches!(
        owner.capture_lease_maintenance_allocation(Instant::now()),
        Err(LifecycleAllocationError::Budget)
    ));
    let observation = owner
        .capture_lease_maintenance_allocation(Instant::now() + Duration::from_secs(2))
        .unwrap();
    let epoch = observation.subject().lease_epoch();
    let moved = SourceWriter::new_for_scan(
        owner.into_catalog_database(),
        SourceId::from_string("source"),
        1,
    )
    .unwrap();
    assert_eq!(moved.current_scan_fence().unwrap().lease_epoch(), epoch);
    assert!(
        matches!(
            moved.validate_lease_maintenance_allocation(&observation),
            Err(LifecycleAllocationError::Unknown)
        ),
        "matching unchanged tuple cannot substitute another issuing writer"
    );
    moved.close().unwrap();
    let reopened = source(&media, &profile);
    assert!(matches!(
        reopened.validate_lease_maintenance_allocation(&observation),
        Err(LifecycleAllocationError::Unknown)
    ));
    assert!(reopened.current_revision().is_err());
    reopened.close().unwrap();
}
#[test]
fn actual_source_rejects_byte_identical_shm_substitution_and_replaced_profile() {
    let temp = tempfile::tempdir().unwrap();
    let media = temp.path().join("media");
    fs::create_dir(&media).unwrap();
    let profile_path = temp.path().join("profile");
    let _root = AppRootGuard::set(profile_path.clone()).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let owner = source(&media, &profile);
    let observation = owner
        .capture_lease_maintenance_allocation(Instant::now() + Duration::from_secs(2))
        .unwrap();
    let shm = profile
        .profile_root()
        .join("source")
        .join(format!("{DB_FILE_NAME}-shm"));
    let parked = profile.profile_root().join("source").join("original-shm");
    fs::rename(&shm, &parked).unwrap();
    fs::copy(&parked, &shm).unwrap();
    assert!(
        owner
            .capture_lease_maintenance_allocation(Instant::now() + Duration::from_secs(2))
            .is_err()
    );
    fs::remove_file(&shm).unwrap();
    fs::rename(&parked, &shm).unwrap();
    owner
        .validate_lease_maintenance_allocation(&observation)
        .unwrap();
    let parked = temp.path().join("original-profile");
    fs::rename(&profile_path, &parked).unwrap();
    fs::create_dir(&profile_path).unwrap();
    assert!(
        owner
            .capture_lease_maintenance_allocation(Instant::now() + Duration::from_secs(2))
            .is_err()
    );
    fs::remove_dir(&profile_path).unwrap();
    fs::rename(&parked, &profile_path).unwrap();
    owner.close().unwrap();
}
#[test]
fn actual_global_unknown_schema_capture_rolls_back_empty_reservation_and_retains_owner() {
    let temp = tempfile::tempdir().unwrap();
    let _root = AppRootGuard::set(temp.path().join("profile")).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let mut writer = global(&profile);
    // Actual unknown external schema fixture, not authenticated production transition SQL.
    let connection = Connection::open(profile.profile_root().join(LIBRARY_DB_FILE_NAME)).unwrap();
    connection
        .execute_batch(
            "CREATE INDEX unqualified_lifecycle_index ON source_lifecycle_references(state)",
        )
        .unwrap();
    let result = writer
        .submit_preparation(request(&writer))
        .unwrap()
        .recv_timeout(Duration::from_secs(3))
        .unwrap();
    assert!(matches!(
        result,
        Err(GlobalPreparationError::LifecycleAllocation(
            LifecycleAllocationError::Unknown
        ))
    ));
    assert_eq!(writer.status(), GlobalLibraryWriterStatus::Available);
    assert_eq!(count(&connection, "source_lifecycle_references"), 0);
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name='unqualified_lifecycle_index'",
                [],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
        1,
        "no schema cleanup/repair"
    );
    connection
        .execute_batch("DROP INDEX unqualified_lifecycle_index")
        .unwrap();
    let reservation = writer
        .submit_preparation(request(&writer))
        .unwrap()
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap();
    assert!(reservation.observation().lifecycle_allocation().is_some());
    reservation
        .abort()
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap();
    assert!(writer.shutdown());
}

#[test]
fn actual_global_lifecycle_policy_keeps_original_scope_until_abort_and_preserves_capped_input() {
    let temp = tempfile::tempdir().unwrap();
    let _root = AppRootGuard::set(temp.path().join("profile")).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let mut writer = global(&profile);
    let before = writer
        .submit_preparation(request(&writer))
        .unwrap()
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap();
    let facts = before.observation().lifecycle_allocation().unwrap().clone();
    assert!(!before.observation().lifecycle_policy_was_entered());
    before
        .abort()
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap();
    let held = writer
        .submit_preparation(request(&writer).with_lifecycle_policy())
        .unwrap()
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap();
    let observation = held.observation().clone();
    assert!(observation.lifecycle_policy_was_entered());
    assert_eq!(observation.lifecycle_allocation(), Some(&facts));
    assert_eq!(
        observation.wal_scope().unwrap().main().identity(),
        facts.main().identity()
    );
    let other = Connection::open(profile.profile_root().join(LIBRARY_DB_FILE_NAME)).unwrap();
    other.busy_timeout(Duration::ZERO).unwrap();
    assert_eq!(
        other
            .execute_batch("BEGIN IMMEDIATE")
            .unwrap_err()
            .sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy)
    );
    held.abort()
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap();
    let after = writer
        .submit_preparation(request(&writer))
        .unwrap()
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap();
    assert_eq!(after.observation().lifecycle_allocation(), Some(&facts));
    assert!(!after.observation().lifecycle_policy_was_entered());
    after
        .abort()
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap();
    assert!(writer.shutdown());
    other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    assert!(
        observation.lifecycle_policy_was_entered(),
        "copied diagnostic stays historical after cleanup"
    );
}

#[test]
fn actual_global_lifecycle_policy_native_sidecar_replacement_denies_reuse() {
    let temp = tempfile::tempdir().unwrap();
    let _root = AppRootGuard::set(temp.path().join("profile")).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let mut writer = global(&profile);
    let held = writer
        .submit_preparation(request(&writer).with_lifecycle_policy())
        .unwrap()
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap();
    let wal = profile
        .profile_root()
        .join(format!("{LIBRARY_DB_FILE_NAME}-wal"));
    let original = wal.with_extension("held-policy-wal");
    fs::rename(&wal, &original).unwrap();
    fs::copy(&original, &wal).unwrap();
    assert_eq!(
        fs::metadata(&wal).unwrap().len(),
        fs::metadata(&original).unwrap().len()
    );
    // The real periodic sweep may already have retired command admission after the
    // substitution. Both rejection before queueing and rejection of queued work
    // preserve the original boundary; neither may dispatch the preference write.
    let queued = writer.replace_ui_preferences_payload("must-not-dispatch".into());
    assert_eq!(
        held.abort().recv_timeout(Duration::from_secs(3)).unwrap(),
        Err(GlobalPreparationError::DatabaseBinding)
    );
    match queued {
        Ok(receiver) => assert!(
            receiver
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .is_err()
        ),
        Err(yield_library::sample_sources::library::GlobalLibraryWriterQueueError::Closed) => {}
        Err(error) => panic!("unexpected queue refusal after original WAL replacement: {error:?}"),
    }
    writer.shutdown_with_report();
    if wal.exists() {
        fs::remove_file(&wal).unwrap();
    }
    fs::rename(&original, &wal).unwrap();
    assert_eq!(
        read(&profile.profile_root().join(LIBRARY_DB_FILE_NAME))
            .query_row(
                "SELECT count(*) FROM metadata WHERE key=?1",
                [yield_library::sample_sources::library::UI_PREFERENCES_KEY],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
        0
    );
    assert_eq!(
        count(
            &read(&profile.profile_root().join(LIBRARY_DB_FILE_NAME)),
            "source_lifecycle_reference_receipts"
        ),
        0
    );
}
