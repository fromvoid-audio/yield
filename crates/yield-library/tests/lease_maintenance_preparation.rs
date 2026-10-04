//! Genuine normal-build owners/SQLite. No journal registration or reserve authority fixtures.
#![cfg(all(target_os = "linux", not(feature = "test-support")))]
use rusqlite::{Connection, ErrorCode, OpenFlags};
use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};
use yield_library::{
    app_dirs::{AppRootGuard, WritableProfileGuard},
    sample_sources::{
        DB_FILE_NAME, SourceDatabase, SourceId, SourceRegistryAuditObservation, SourceWriter,
    },
};
fn source(media: &Path, profile: &WritableProfileGuard, child: &str) -> SourceWriter {
    SourceWriter::new_for_scan(
        SourceDatabase::open_for_catalog_with_profile_guard(media, profile, Path::new(child))
            .unwrap(),
        SourceId::from_string(child),
        1,
    )
    .unwrap()
}
fn row(connection: &Connection) -> (String, i64, i64, String) {
    connection
        .query_row(
            "SELECT owner_identity,lease_epoch,expires_at,state FROM source_lease",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap()
}
#[test]
fn two_actual_never_audited_sources_hold_main_without_renewal_and_abort_to_original_writer() {
    let temp = tempfile::tempdir().unwrap();
    let _root = AppRootGuard::set(temp.path().join("profile")).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let media_a = temp.path().join("media-a");
    let media_b = temp.path().join("media-b");
    fs::create_dir(&media_a).unwrap();
    fs::create_dir(&media_b).unwrap();
    let a = source(&media_a, &profile, "a");
    let b = source(&media_b, &profile, "b");
    let fence_a = a.current_scan_fence().unwrap();
    let fence_b = b.current_scan_fence().unwrap();
    let path_a = profile.profile_root().join("a").join(DB_FILE_NAME);
    let path_b = profile.profile_root().join("b").join(DB_FILE_NAME);
    let other_a = Connection::open_with_flags(path_a, OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    let other_b = Connection::open_with_flags(path_b, OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    other_a.busy_timeout(Duration::ZERO).unwrap();
    other_b.busy_timeout(Duration::ZERO).unwrap();
    let rows = (row(&other_a), row(&other_b));
    let deadline = Instant::now() + Duration::from_secs(2);
    let obs_a = a.capture_lease_maintenance_allocation(deadline).unwrap();
    let obs_b = b.capture_lease_maintenance_allocation(deadline).unwrap();
    assert_eq!(
        obs_a.subject().audit(),
        &SourceRegistryAuditObservation::Unavailable {}
    );
    assert_eq!(
        obs_b.subject().audit(),
        &SourceRegistryAuditObservation::Unavailable {}
    );
    let prepared_a = a.prepare_lease_maintenance(obs_a, deadline).unwrap();
    let prepared_b = b.prepare_lease_maintenance(obs_b, deadline).unwrap();
    assert_ne!(prepared_a.nonce(), prepared_b.nonce());
    assert_eq!(prepared_a.deadline(), deadline);
    prepared_a.revalidate().unwrap();
    prepared_b.revalidate().unwrap();
    for other in [&other_a, &other_b] {
        assert_eq!(
            other
                .execute_batch("BEGIN IMMEDIATE")
                .unwrap_err()
                .sqlite_error_code(),
            Some(ErrorCode::DatabaseBusy)
        );
        assert_eq!(
            other
                .query_row(
                    "SELECT count(*) FROM source_participant_curation_receipts",
                    [],
                    |r| r.get::<_, u64>(0)
                )
                .unwrap(),
            0
        );
    }
    assert_eq!((row(&other_a), row(&other_b)), rows);
    let a = prepared_a.abort().unwrap();
    let b = prepared_b.abort().unwrap();
    assert_eq!(a.current_scan_fence().unwrap(), fence_a);
    assert_eq!(b.current_scan_fence().unwrap(), fence_b);
    assert!(a.current_revision().is_err());
    assert!(b.current_revision().is_err());
    assert_eq!((row(&other_a), row(&other_b)), rows);
    other_a.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    other_b.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    a.close().unwrap();
    b.close().unwrap();
}
#[test]
fn actual_busy_empty_preparation_restores_owner_and_drop_closes_before_lease_release() {
    let temp = tempfile::tempdir().unwrap();
    let media = temp.path().join("media");
    fs::create_dir(&media).unwrap();
    let _root = AppRootGuard::set(temp.path().join("profile")).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let owner = source(&media, &profile, "source");
    let path = profile.profile_root().join("source").join(DB_FILE_NAME);
    let other = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    other.busy_timeout(Duration::ZERO).unwrap();
    let original = row(&other);
    let deadline = Instant::now() + Duration::from_secs(2);
    let observation = owner
        .capture_lease_maintenance_allocation(deadline)
        .unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    let failure = owner
        .prepare_lease_maintenance(observation, deadline)
        .unwrap_err();
    other.execute_batch("ROLLBACK").unwrap();
    let owner = failure.into_writer().unwrap();
    assert_eq!(row(&other), original);
    let deadline = Instant::now() + Duration::from_secs(2);
    let observation = owner
        .capture_lease_maintenance_allocation(deadline)
        .unwrap();
    let held = owner
        .prepare_lease_maintenance(observation, deadline)
        .unwrap();
    drop(held);
    other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    assert_eq!(row(&other).3, "released");
}
