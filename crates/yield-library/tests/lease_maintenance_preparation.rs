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

#[test]
fn original_pre_audit_preparation_captures_exact_registration_and_rejects_foreign_or_reused_binding()
 {
    use yield_library::operation_journal::source_lease_binding::StoredSourceLeaseFence;
    use yield_library::sample_sources::db::SourceRegistryRegistrationPhase;
    let temp = tempfile::tempdir().unwrap();
    let _root = AppRootGuard::set(temp.path().join("profile")).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let media = temp.path().join("media");
    let foreign_media = temp.path().join("foreign");
    fs::create_dir(&media).unwrap();
    fs::create_dir(&foreign_media).unwrap();
    let owner = source(&media, &profile, "source");
    let registration = owner.catalog_database().prepare_source_authority().unwrap();
    let foreign = source(&foreign_media, &profile, "foreign");
    let foreign_registration = foreign
        .catalog_database()
        .prepare_source_authority()
        .unwrap();
    let before = owner.current_scan_fence().unwrap();
    assert!(owner.current_revision().is_err());
    let deadline = Instant::now() + Duration::from_secs(2);
    let observation = owner
        .capture_lease_maintenance_allocation(deadline)
        .unwrap();
    let prepared = owner
        .prepare_lease_maintenance(observation, deadline)
        .unwrap();
    let binding = prepared.capture_lease_binding(&registration).unwrap();
    assert_eq!(binding.stored_fence().preparation_nonce(), prepared.nonce());
    let subject = binding.stored_fence().subject();
    assert_eq!(
        subject.audit(),
        &SourceRegistryAuditObservation::Unavailable {}
    );
    assert_eq!(subject.scan_revision(), before.revision());
    assert_eq!(
        subject.registration().unwrap().phase(),
        SourceRegistryRegistrationPhase::Prepared
    );
    let bytes = binding.stored_fence().canonical_bytes().unwrap();
    assert_eq!(
        StoredSourceLeaseFence::decode(&bytes).unwrap(),
        *binding.stored_fence()
    );
    prepared
        .validate_lease_binding(&binding, &registration)
        .unwrap();
    assert!(
        prepared
            .capture_lease_binding(&foreign_registration)
            .is_err()
    );
    assert!(
        prepared
            .validate_lease_binding(&binding, &foreign_registration)
            .is_err()
    );
    let owner = prepared.abort().unwrap();
    assert_eq!(owner.current_scan_fence().unwrap(), before);
    let deadline = Instant::now() + Duration::from_secs(2);
    let observation = owner
        .capture_lease_maintenance_allocation(deadline)
        .unwrap();
    let prepared = owner
        .prepare_lease_maintenance(observation, deadline)
        .unwrap();
    assert!(
        prepared
            .validate_lease_binding(&binding, &registration)
            .is_err()
    );
    let new_binding = prepared.capture_lease_binding(&registration).unwrap();
    assert_ne!(
        new_binding.stored_fence().preparation_nonce(),
        binding.stored_fence().preparation_nonce()
    );
    // A new preparation cannot reuse the previous capture, even with unchanged stored facts.
    let owner = prepared.abort().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let observation = owner
        .capture_lease_maintenance_allocation(deadline)
        .unwrap();
    let prepared = owner
        .prepare_lease_maintenance(observation, deadline)
        .unwrap();
    assert!(
        prepared
            .validate_lease_binding(&new_binding, &registration)
            .is_err()
    );
    drop(registration);
    drop(foreign_registration);
    prepared.abort().unwrap().close().unwrap();
    foreign.close().unwrap();
    let owner = source(&media, &profile, "source");
    let registration = owner.catalog_database().prepare_source_authority().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let observation = owner
        .capture_lease_maintenance_allocation(deadline)
        .unwrap();
    let prepared = owner
        .prepare_lease_maintenance(observation, deadline)
        .unwrap();
    assert!(
        prepared
            .validate_lease_binding(&binding, &registration)
            .is_err()
    );
    assert_ne!(
        prepared
            .capture_lease_binding(&registration)
            .unwrap()
            .stored_fence()
            .subject()
            .lease_epoch(),
        binding.stored_fence().subject().lease_epoch()
    );
    drop(registration);
    prepared.abort().unwrap().close().unwrap();
}

#[test]
fn captured_binding_refuses_replaced_root_and_elapsed_original_deadline() {
    let temp = tempfile::tempdir().unwrap();
    let _root = AppRootGuard::set(temp.path().join("profile")).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let media = temp.path().join("media");
    fs::create_dir(&media).unwrap();
    let owner = source(&media, &profile, "source");
    let registration = owner.catalog_database().prepare_source_authority().unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    let observation = owner
        .capture_lease_maintenance_allocation(deadline)
        .unwrap();
    let prepared = owner
        .prepare_lease_maintenance(observation, deadline)
        .unwrap();
    let binding = prepared.capture_lease_binding(&registration).unwrap();
    let retained = temp.path().join("original-media");
    fs::rename(&media, &retained).unwrap();
    fs::create_dir(&media).unwrap();
    assert!(
        prepared
            .validate_lease_binding(&binding, &registration)
            .is_err()
    );
    assert!(prepared.capture_lease_binding(&registration).is_err());
    fs::remove_dir(&media).unwrap();
    fs::rename(&retained, &media).unwrap();
    prepared
        .validate_lease_binding(&binding, &registration)
        .unwrap();
    std::thread::sleep(
        deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(5),
    );
    assert!(
        prepared
            .validate_lease_binding(&binding, &registration)
            .is_err()
    );
    assert!(prepared.capture_lease_binding(&registration).is_err());
    drop(registration);
    prepared.abort().unwrap().close().unwrap();
}

#[test]
fn normal_parked_never_audited_lease_binding_preserves_original_owner_and_registration() {
    let temp = tempfile::tempdir().unwrap();
    let _root = AppRootGuard::set(temp.path().join("profile")).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let media = temp.path().join("media");
    fs::create_dir(&media).unwrap();
    let a = source(&media, &profile, "parked-a");
    let b = source(&media, &profile, "parked-b");
    let ar = a.catalog_database().prepare_source_authority().unwrap();
    let br = b.catalog_database().prepare_source_authority().unwrap();
    let other =
        Connection::open(profile.profile_root().join("parked-a").join(DB_FILE_NAME)).unwrap();
    other.busy_timeout(Duration::ZERO).unwrap();
    let before = row(&other);
    let scan = a.current_scan_fence().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let observation = a.capture_lease_maintenance_allocation(deadline).unwrap();
    let a = a.prepare_lease_maintenance(observation, deadline).unwrap();
    let binding = a.capture_lease_binding(&ar).unwrap();
    assert!(matches!(
        binding.stored_fence().subject().audit(),
        SourceRegistryAuditObservation::Unavailable {}
    ));
    a.revalidate_owner().unwrap();
    a.validate_lease_binding(&binding, &ar).unwrap();
    assert!(a.validate_lease_binding(&binding, &br).is_err());
    assert_eq!(
        other
            .execute_batch("BEGIN IMMEDIATE")
            .unwrap_err()
            .sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy)
    );
    assert_eq!(row(&other), before);
    let a = a.abort().unwrap();
    assert_eq!(a.current_scan_fence().unwrap(), scan);
    assert!(a.current_revision().is_err());
    assert_eq!(row(&other), before);
    other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    drop(ar);
    drop(br);
    a.close().unwrap();
    b.close().unwrap();
}
