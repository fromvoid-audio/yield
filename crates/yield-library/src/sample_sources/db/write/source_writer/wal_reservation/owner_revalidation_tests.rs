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

#[cfg(target_os = "linux")]
#[test]
fn original_wal_binding_revalidates_without_ordinary_sql_and_rejects_new_nonce() {
    let (_fixture, writer) = fixture();
    let revision = writer.current_revision().unwrap();
    let registration = writer.database.prepare_source_authority().unwrap();
    let other = external(&writer);
    let lease_before = lease_row(&other);
    let metadata_before = metadata(&other);
    let main_changes = writer.database.connection.total_changes();
    let control_changes = writer
        .database
        .source_lease_control_total_changes_for_tests()
        .unwrap()
        .unwrap();
    let handle = unsafe { writer.database.connection.handle() };
    let preparation = writer
        .prepare_wal_reservation(&revision, Instant::now() + Duration::from_secs(2))
        .unwrap();
    let binding = preparation.capture_wal_binding(&registration).unwrap();
    assert_eq!(
        binding.subject().registration().unwrap().phase(),
        crate::sample_sources::SourceRegistryRegistrationPhase::Prepared
    );
    assert_eq!(
        binding.subject().profile_acquisition().to_string(),
        _fixture._profile.owner_identity()
    );
    assert_eq!(binding.main().identity(), revision.database_identity());
    assert_ne!(binding.main().identity(), binding.wal().identity());
    assert_ne!(binding.wal().identity(), binding.shm().identity());
    assert_eq!(binding.main().device(), binding.shm().device());
    assert!(binding.descriptor_bytes().len() <= super::SOURCE_WAL_BINDING_MAX_BYTES);
    let attempts = Arc::new(AtomicUsize::new(0));
    let calls = attempts.clone();
    preparation
        .writer()
        .database
        .connection
        .authorizer(Some(move |_: AuthContext<'_>| {
            calls.fetch_add(1, Ordering::SeqCst);
            Authorization::Deny
        }));
    for _ in 0..5 {
        preparation
            .validate_wal_binding(&binding, &registration)
            .unwrap();
        busy(&other);
    }
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
    assert_eq!(
        preparation.writer().database.connection.total_changes(),
        main_changes
    );
    assert_eq!(
        preparation
            .writer()
            .database
            .source_lease_control_total_changes_for_tests()
            .unwrap()
            .unwrap(),
        control_changes
    );
    assert_eq!(lease_row(&other), lease_before);
    assert_eq!(metadata(&other), metadata_before);
    preparation
        .writer()
        .database
        .connection
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
    let writer = preparation.abort().unwrap();
    assert_eq!(unsafe { writer.database.connection.handle() }, handle);
    let again = writer
        .prepare_wal_reservation(&revision, Instant::now() + Duration::from_secs(2))
        .unwrap();
    assert!(matches!(
        again.validate_wal_binding(&binding, &registration),
        Err(SourceWalPreparationError::BindingUnavailable)
    ));
    again.capture_wal_binding(&registration).unwrap();
    let writer = again.abort().unwrap();
    other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    drop(registration);
    writer.close().unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn original_wal_binding_rejects_foreign_registration_and_native_sidecar_replacements() {
    let (fixture, writer) = fixture();
    let foreign = SourceDatabase::open_for_catalog_with_profile_guard(
        &fixture.media,
        &fixture._profile,
        Path::new("foreign-wal-binding"),
    )
    .unwrap();
    let wrong = foreign.prepare_source_authority().unwrap();
    let actual = writer.database.prepare_source_authority().unwrap();
    let revision = writer.current_revision().unwrap();
    let other = external(&writer);
    let path = writer.database.db_path.clone();
    let preparation = writer
        .prepare_wal_reservation(&revision, Instant::now() + Duration::from_secs(2))
        .unwrap();
    assert!(matches!(
        preparation.capture_wal_binding(&wrong),
        Err(SourceWalPreparationError::BindingUnavailable)
    ));
    let binding = preparation.capture_wal_binding(&actual).unwrap();
    assert!(matches!(
        preparation.validate_wal_binding(&binding, &wrong),
        Err(SourceWalPreparationError::BindingUnavailable)
    ));
    for suffix in ["-wal", "-shm"] {
        let sidecar = std::path::PathBuf::from(format!("{}{suffix}", path.display()));
        let saved = sidecar.with_extension("original");
        fs::rename(&sidecar, &saved).unwrap();
        let len = fs::metadata(&saved).unwrap().len();
        let replacement = fs::File::create(&sidecar).unwrap();
        replacement.set_len(len).unwrap();
        assert!(matches!(
            preparation.validate_wal_binding(&binding, &actual),
            Err(SourceWalPreparationError::BindingUnavailable)
        ));
        assert!(matches!(
            preparation.capture_wal_binding(&actual),
            Err(SourceWalPreparationError::BindingUnavailable)
        ));
        assert!(!preparation.writer().database.connection.is_autocommit());
        fs::remove_file(&sidecar).unwrap();
        fs::rename(&saved, &sidecar).unwrap();
        preparation.validate_wal_binding(&binding, &actual).unwrap();
        busy(&other);
    }
    let writer = preparation.abort().unwrap();
    drop(actual);
    drop(wrong);
    foreign.close().unwrap();
    writer.close().unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn original_wal_binding_rejects_unknown_schema_without_releasing_main() {
    let (_fixture, writer) = fixture();
    writer
        .database
        .connection
        .execute_batch("CREATE TABLE unsupported_binding_schema(x INTEGER)")
        .unwrap();
    let revision = writer.current_revision().unwrap();
    let actual = writer.database.prepare_source_authority().unwrap();
    let other = external(&writer);
    let preparation = writer
        .prepare_wal_reservation(&revision, Instant::now() + Duration::from_secs(2))
        .unwrap();
    assert!(matches!(
        preparation.capture_wal_binding(&actual),
        Err(SourceWalPreparationError::BindingUnavailable)
    ));
    busy(&other);
    let writer = preparation.abort().unwrap();
    writer
        .database
        .connection
        .execute_batch("DROP TABLE unsupported_binding_schema")
        .unwrap();
    let preparation = writer
        .prepare_wal_reservation(&revision, Instant::now() + Duration::from_secs(2))
        .unwrap();
    preparation.capture_wal_binding(&actual).unwrap();
    let writer = preparation.abort().unwrap();
    drop(actual);
    writer.close().unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn original_wal_binding_never_extends_original_deadline_or_releases_on_expiry() {
    let (_fixture, writer) = fixture();
    let revision = writer.current_revision().unwrap();
    let registration = writer.database.prepare_source_authority().unwrap();
    let other = external(&writer);
    let deadline = Instant::now() + Duration::from_secs(1);
    let preparation = writer.prepare_wal_reservation(&revision, deadline).unwrap();
    let binding = preparation.capture_wal_binding(&registration).unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    for _ in 0..3 {
        assert!(matches!(
            preparation.validate_wal_binding(&binding, &registration),
            Err(SourceWalPreparationError::Deadline)
        ));
        assert!(matches!(
            preparation.capture_wal_binding(&registration),
            Err(SourceWalPreparationError::Deadline)
        ));
        assert!(matches!(
            preparation.validate_registered_wal_binding(&binding, &registration),
            Err(SourceWalPreparationError::Deadline)
        ));
        assert_eq!(preparation.deadline, deadline);
        busy(&other);
    }
    let writer = preparation.abort().unwrap();
    drop(registration);
    writer.close().unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn original_wal_binding_refuses_unknown_runtime_and_missing_completed_audit() {
    unsafe extern "C" fn callback(
        context: *mut rusqlite::ffi::sqlite3_context,
        _: std::ffi::c_int,
        _: *mut *mut rusqlite::ffi::sqlite3_value,
    ) {
        // SAFETY: SQLite supplies the callback's live result context; no borrowed state.
        unsafe { rusqlite::ffi::sqlite3_result_int(context, 1) };
    }
    for runtime_fault in [true, false] {
        let (_fixture, writer) = fixture();
        let revision = writer.current_revision().unwrap();
        let registration = writer.database.prepare_source_authority().unwrap();
        let other = external(&writer);
        let before = metadata(&other);
        let preparation = writer
            .prepare_wal_reservation(&revision, Instant::now() + Duration::from_secs(2))
            .unwrap();
        preparation.capture_wal_binding(&registration).unwrap();
        // Private fault injection on the original connection. Public preparations expose no SQL.
        if runtime_fault {
            // SAFETY: actual exclusively retained connection; callback has static lifetime
            // and no application state. Only this private test registers it.
            let code = unsafe {
                rusqlite::ffi::sqlite3_create_function_v2(
                    preparation.writer().database.connection.handle(),
                    c"unsupported_wal_binding_callback".as_ptr(),
                    0,
                    rusqlite::ffi::SQLITE_UTF8,
                    std::ptr::null_mut(),
                    Some(callback),
                    None,
                    None,
                    None,
                )
            };
            assert_eq!(code, rusqlite::ffi::SQLITE_OK);
        } else {
            preparation
                .writer()
                .database
                .connection
                .execute(
                    "DELETE FROM metadata WHERE key=?1",
                    [crate::sample_sources::db::META_LAST_AUTHORITATIVE_AUDIT_REVISION],
                )
                .unwrap();
        }
        assert!(matches!(
            preparation.capture_wal_binding(&registration),
            Err(SourceWalPreparationError::BindingUnavailable)
        ));
        busy(&other);
        assert_eq!(
            metadata(&other),
            before,
            "fault did not publish an audit change"
        );
        let writer = preparation.abort().unwrap();
        if runtime_fault {
            // SAFETY: same returned connection, no outstanding statements or callback state.
            let code = unsafe {
                rusqlite::ffi::sqlite3_create_function_v2(
                    writer.database.connection.handle(),
                    c"unsupported_wal_binding_callback".as_ptr(),
                    0,
                    rusqlite::ffi::SQLITE_UTF8,
                    std::ptr::null_mut(),
                    None,
                    None,
                    None,
                    None,
                )
            };
            assert_eq!(code, rusqlite::ffi::SQLITE_OK);
        }
        assert_eq!(writer.current_revision().unwrap(), revision);
        let preparation = writer
            .prepare_wal_reservation(&revision, Instant::now() + Duration::from_secs(2))
            .unwrap();
        preparation.capture_wal_binding(&registration).unwrap();
        let writer = preparation.abort().unwrap();
        drop(registration);
        writer.close().unwrap();
    }
}

#[cfg(target_os = "linux")]
#[test]
fn registered_boundary_refuses_prepared_without_ordinary_sql_or_owner_release() {
    let (_fixture, writer) = fixture();
    let registration = writer.database.prepare_source_authority().unwrap();
    let revision = writer.current_revision().unwrap();
    let other = external(&writer);
    let before = (lease_row(&other), metadata(&other));
    let control = writer
        .database
        .source_lease_control_total_changes_for_tests()
        .unwrap()
        .unwrap();
    let handle = unsafe { writer.database.connection.handle() };
    let preparation = writer
        .prepare_wal_reservation(&revision, Instant::now() + Duration::from_secs(2))
        .unwrap();
    let binding = preparation.capture_wal_binding(&registration).unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let attempts = Arc::clone(&count);
    preparation
        .writer()
        .database
        .connection
        .authorizer(Some(move |_: AuthContext<'_>| {
            attempts.fetch_add(1, Ordering::SeqCst);
            Authorization::Deny
        }));
    for _ in 0..8 {
        assert!(matches!(
            preparation.validate_registered_wal_binding(&binding, &registration),
            Err(SourceWalPreparationError::BindingUnavailable)
        ));
        preparation
            .validate_wal_binding(&binding, &registration)
            .unwrap();
    }
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert_eq!(
        preparation
            .writer()
            .database
            .source_lease_control_total_changes_for_tests()
            .unwrap()
            .unwrap(),
        control
    );
    assert_eq!((lease_row(&other), metadata(&other)), before);
    busy(&other);
    preparation
        .writer()
        .database
        .connection
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
    let writer = preparation.abort().unwrap();
    assert_eq!(unsafe { writer.database.connection.handle() }, handle);
    assert_eq!(writer.current_revision().unwrap(), revision);
    assert_eq!((lease_row(&other), metadata(&other)), before);
    other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    drop(registration);
    writer.close().unwrap();
}
