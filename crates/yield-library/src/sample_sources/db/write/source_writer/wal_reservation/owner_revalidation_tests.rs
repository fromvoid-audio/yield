//! Real retained owners/SQLite; these tests supply no physical capacity qualification.
use super::*;
use crate::app_dirs::{AppRootGuard, WritableProfileGuard};
use crate::sample_sources::{SourceDatabase, SourceId};
use rusqlite::{
    Connection, ErrorCode,
    hooks::{AuthContext, Authorization},
};
use std::{
    fs,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

struct Fixture {
    _temp: tempfile::TempDir,
    _root: AppRootGuard,
    _profile: WritableProfileGuard,
    media: std::path::PathBuf,
    profile: std::path::PathBuf,
}

fn fixture() -> (Fixture, SourceWriter) {
    let temp = tempfile::tempdir().unwrap();
    let media = temp.path().join("media");
    fs::create_dir(&media).unwrap();
    let profile_path = temp.path().join("profile");
    let root = AppRootGuard::set(profile_path.clone()).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        &media,
        &profile,
        Path::new("held-owner-validation"),
    )
    .unwrap();
    let writer =
        SourceWriter::new_for_scan(database, SourceId::from_string("held-owner-validation"), 1)
            .unwrap();
    let generation = writer.begin_scan_directory_truth_generation(0).unwrap();
    let fence = writer.current_scan_fence().unwrap();
    let batch = writer.database.write_batch().unwrap();
    writer
        .commit_scan_audit_completion(
            &fence,
            batch,
            generation,
            1,
            fence.root_identity().clone(),
            None,
        )
        .unwrap();
    (
        Fixture {
            _temp: temp,
            _root: root,
            _profile: profile,
            media,
            profile: profile_path,
        },
        writer,
    )
}

fn external(writer: &SourceWriter) -> Connection {
    let connection = Connection::open_with_flags(
        &writer.database.db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
    )
    .unwrap();
    connection.busy_timeout(Duration::ZERO).unwrap();
    connection
}
fn busy(connection: &Connection) {
    let error = connection.execute_batch("BEGIN IMMEDIATE").unwrap_err();
    assert_eq!(error.sqlite_error_code(), Some(ErrorCode::DatabaseBusy));
}
fn lease_row(connection: &Connection) -> (String, i64, i64, i64, String) {
    connection.query_row("SELECT owner_identity,lease_epoch,acquired_at,expires_at,state FROM source_lease WHERE singleton=1",
        [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap()
}
fn metadata(connection: &Connection) -> Vec<(String, String)> {
    connection
        .prepare("SELECT key,value FROM metadata ORDER BY key")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn held_owner_revalidation_never_queries_ordinary_connection_or_changes_rows() {
    let (_fixture, writer) = fixture();
    let revision = writer.current_revision().unwrap();
    let other = external(&writer);
    let main_before = writer.database.connection.total_changes();
    let control_before = writer
        .database
        .source_lease_control_total_changes_for_tests()
        .unwrap()
        .unwrap();
    let lease_before = lease_row(&other);
    let metadata_before = metadata(&other);
    // SAFETY: live owned connection; observe identity only, never dereference.
    let handle = unsafe { writer.database.connection.handle() };
    let preparation = writer
        .prepare_wal_reservation(&revision, Instant::now() + Duration::from_secs(2))
        .unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let attempts = count.clone();
    preparation
        .held
        .writer()
        .database
        .connection
        .authorizer(Some(move |_: AuthContext<'_>| {
            attempts.fetch_add(1, Ordering::SeqCst);
            Authorization::Deny
        }));
    for _ in 0..8 {
        preparation.revalidate_owner().unwrap();
    }
    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "no ordinary SQL was even attempted"
    );
    assert_eq!(
        preparation
            .held
            .writer()
            .database
            .connection
            .total_changes(),
        main_before
    );
    assert_eq!(
        preparation
            .held
            .writer()
            .database
            .source_lease_control_total_changes_for_tests()
            .unwrap()
            .unwrap(),
        control_before
    );
    assert_eq!(lease_row(&other), lease_before);
    assert_eq!(metadata(&other), metadata_before);
    assert_eq!(preparation.revision(), &revision);
    busy(&other);
    preparation
        .held
        .writer()
        .database
        .connection
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
    let writer = preparation.abort().unwrap();
    // SAFETY: the confirmed abort returned the same live owned connection.
    assert_eq!(
        unsafe { writer.database.connection.handle() },
        handle,
        "exact connection returned"
    );
    assert_eq!(writer.current_revision().unwrap(), revision);
    other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    writer
        .prepare_wal_reservation(&revision, Instant::now() + Duration::from_secs(2))
        .unwrap()
        .abort()
        .unwrap()
        .close()
        .unwrap();
}

#[test]
fn held_owner_revalidation_original_deadline_stays_expired_and_exclusive_until_abort() {
    let (_fixture, writer) = fixture();
    let revision = writer.current_revision().unwrap();
    let other = external(&writer);
    let deadline = Instant::now() + Duration::from_secs(1);
    let preparation = writer.prepare_wal_reservation(&revision, deadline).unwrap();
    preparation.revalidate_owner().unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    for _ in 0..3 {
        assert!(preparation.is_expired());
        assert_eq!(preparation.deadline, deadline);
        assert!(matches!(
            preparation.revalidate_owner(),
            Err(SourceWalPreparationError::Deadline)
        ));
        busy(&other);
    }
    let writer = preparation.abort().unwrap();
    other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    writer
        .prepare_wal_reservation(&revision, Instant::now() + Duration::from_secs(2))
        .unwrap()
        .abort()
        .unwrap()
        .close()
        .unwrap();
}

#[test]
fn held_owner_revalidation_rejects_actual_media_root_substitution_without_unlocking() {
    let (fixture, writer) = fixture();
    let revision = writer.current_revision().unwrap();
    let other = external(&writer);
    let preparation = writer
        .prepare_wal_reservation(&revision, Instant::now() + Duration::from_secs(2))
        .unwrap();
    let saved = fixture.media.with_extension("saved");
    fs::rename(&fixture.media, &saved).unwrap();
    assert!(matches!(
        preparation.revalidate_owner(),
        Err(SourceWalPreparationError::Owner(_))
    ));
    fs::create_dir(&fixture.media).unwrap();
    assert!(matches!(
        preparation.revalidate_owner(),
        Err(SourceWalPreparationError::Owner(_))
    ));
    busy(&other);
    fs::remove_dir(&fixture.media).unwrap();
    fs::rename(saved, &fixture.media).unwrap();
    preparation.revalidate_owner().unwrap();
    preparation.abort().unwrap().close().unwrap();
}

#[test]
fn held_owner_revalidation_rejects_actual_profile_substitution_without_unlocking() {
    let (fixture, writer) = fixture();
    let revision = writer.current_revision().unwrap();
    let other = external(&writer);
    let preparation = writer
        .prepare_wal_reservation(&revision, Instant::now() + Duration::from_secs(2))
        .unwrap();
    let saved = fixture.profile.with_extension("saved");
    fs::rename(&fixture.profile, &saved).unwrap();
    assert!(matches!(
        preparation.revalidate_owner(),
        Err(SourceWalPreparationError::Owner(_))
    ));
    fs::create_dir(&fixture.profile).unwrap();
    assert!(matches!(
        preparation.revalidate_owner(),
        Err(SourceWalPreparationError::Owner(_))
    ));
    // Moving the entire database directory can make the observer's SQLite
    // sidecar lookup fail with IOERR instead of BUSY. Check retained transaction
    // custody directly here and actual lock exclusion after namespace restore.
    assert!(
        !preparation
            .held
            .writer()
            .database
            .connection
            .is_autocommit()
    );
    fs::remove_dir(&fixture.profile).unwrap();
    fs::rename(saved, &fixture.profile).unwrap();
    busy(&other);
    preparation.revalidate_owner().unwrap();
    preparation.abort().unwrap().close().unwrap();
}

#[test]
fn held_owner_revalidation_rejects_actual_main_file_substitution_without_unlocking() {
    let (_fixture, writer) = fixture();
    let revision = writer.current_revision().unwrap();
    let other = external(&writer);
    let path = writer.database.db_path.clone();
    let preparation = writer
        .prepare_wal_reservation(&revision, Instant::now() + Duration::from_secs(2))
        .unwrap();
    let saved = path.with_extension("saved");
    fs::rename(&path, &saved).unwrap();
    assert!(matches!(
        preparation.revalidate_owner(),
        Err(SourceWalPreparationError::Owner(_))
    ));
    fs::write(&path, b"unrelated replacement").unwrap();
    assert!(matches!(
        preparation.revalidate_owner(),
        Err(SourceWalPreparationError::Owner(_))
    ));
    busy(&other);
    fs::remove_file(&path).unwrap();
    fs::rename(saved, &path).unwrap();
    preparation.revalidate_owner().unwrap();
    preparation.abort().unwrap().close().unwrap();
}

#[test]
fn failed_empty_reservation_cannot_retire_without_original_durable_drain() {
    let (_fixture, writer) = fixture();
    let revision = writer.current_revision().unwrap();
    let mut authority = writer.database.prepare_source_authority().unwrap();
    let other = external(&writer);
    let preparation = writer
        .prepare_wal_reservation(&revision, Instant::now() + Duration::from_secs(2))
        .unwrap();
    preparation
        .held
        .writer()
        .database
        .connection
        .authorizer(Some(|_: AuthContext<'_>| Authorization::Deny));
    let failure = match preparation.abort() {
        Err(failure) => failure,
        Ok(_) => panic!("real rollback denial must keep owner private"),
    };
    busy(&other);
    assert_eq!(
        failure.retained_database_identity(),
        Some(revision.database_identity())
    );
    assert!(failure.source_authority_close_ready());
    assert!(matches!(
        failure.close_for_retirement(&mut authority),
        Err(SourceAuthorityError::InvalidLifecycle)
    ));
    other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    // Closing a consumed resource does not manufacture a registration or retirement ack.
    drop(authority);
}

#[test]
fn failed_empty_reservation_cannot_use_another_sources_actual_guard() {
    let (fixture, writer) = fixture();
    let revision = writer.current_revision().unwrap();
    let other = external(&writer);
    let different = SourceDatabase::open_for_catalog_with_profile_guard(
        &fixture.media,
        &fixture._profile,
        Path::new("different-retirement-owner"),
    )
    .unwrap();
    let mut wrong_authority = different.prepare_source_authority().unwrap();
    let preparation = writer
        .prepare_wal_reservation(&revision, Instant::now() + Duration::from_secs(2))
        .unwrap();
    preparation
        .held
        .writer()
        .database
        .connection
        .authorizer(Some(|_: AuthContext<'_>| Authorization::Deny));
    let failure = match preparation.abort() {
        Err(failure) => failure,
        Ok(_) => panic!("real rollback denial must keep owner private"),
    };
    busy(&other);
    assert!(matches!(
        failure.close_for_retirement(&mut wrong_authority),
        Err(SourceAuthorityError::GuardMismatch)
    ));
    other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    drop(wrong_authority);
    drop(different);
}
