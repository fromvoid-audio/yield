//! Real SQLite/source lifecycle tests; no journal admission or physical qualification.
use super::*;
use crate::app_dirs::{AppRootGuard, WritableProfileGuard};
use crate::sample_sources::db::source_authority::{
    SourceAuthorityRequest, SourceAuthorityUseProof,
};
use crate::sample_sources::{SourceDatabase, db::source_lease::SourceLeaseError};
use crate::sample_sources::{SourceId, db::write::SourceWriter};
use rusqlite::Connection;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
use std::sync::Arc;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use std::sync::atomic::Ordering;
use std::{path::Path, time::Duration};
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use std::{sync::atomic::AtomicBool, time::Instant};

struct Fixture {
    _temp: tempfile::TempDir,
    _root: AppRootGuard,
    _profile: WritableProfileGuard,
}
fn fixture() -> (Fixture, SourceWriter) {
    let temp = tempfile::tempdir().unwrap();
    let media = temp.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let root = AppRootGuard::set(temp.path().join("profile")).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let db = SourceDatabase::open_for_catalog_with_profile_guard(
        &media,
        &profile,
        Path::new("registered-custody"),
    )
    .unwrap();
    let writer =
        SourceWriter::new_for_scan(db, SourceId::from_string("registered-custody"), 1).unwrap();
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
        },
        writer,
    )
}
fn registered(
    db: &SourceDatabase,
) -> (
    SourceAuthorityRegistration,
    SourceAuthorityUseProof,
    Arc<SourceAuthorityRequest>,
) {
    let registration = db.prepare_source_authority().unwrap();
    // Exercise the actual confirmation lifecycle only. No production capacity provider is installed.
    let envelope = registration.journal_request().unwrap();
    let request = envelope.request;
    let proof = request.accept_confirmation(envelope.confirmation).unwrap();
    (registration, proof, request)
}
fn count(request: &SourceAuthorityRequest) -> usize {
    request.registered_uses_for_tests()
}
fn external(writer: &SourceWriter) -> Connection {
    let connection = Connection::open(&writer.database.db_path).unwrap();
    connection.busy_timeout(Duration::ZERO).unwrap();
    connection
}
fn lease_state(connection: &Connection) -> String {
    connection
        .query_row(
            "SELECT state FROM source_lease WHERE singleton=1",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn original_registered_guard_rejects_prepared_and_releases_exact_count() {
    let (_fixture, writer) = fixture();
    let registration = writer.database.prepare_source_authority().unwrap();
    let envelope = registration.journal_request().unwrap();
    let request = envelope.request;
    assert!(
        registration
            .retain_registered_database_boundary(&writer.database)
            .is_err()
    );
    assert_eq!(count(&request), 0);
    let proof = request.accept_confirmation(envelope.confirmation).unwrap();
    let custody = registration
        .retain_registered_database_boundary(&writer.database)
        .unwrap();
    assert_eq!(count(&request), 1);
    assert!(custody.close_ready(&writer.database));
    custody.validate(&writer.database, &registration).unwrap();
    request.begin_drain(proof.into_drain_marker()).unwrap();
    assert!(custody.validate(&writer.database, &registration).is_err());
    assert_eq!(count(&request), 1);
    drop(custody);
    assert_eq!(count(&request), 0);
    drop(registration);
    writer.close().unwrap();
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
#[test]
fn original_reservation_holds_registered_use_until_confirmed_abort() {
    let (_fixture, writer) = fixture();
    let revision = writer.current_revision().unwrap();
    let other = external(&writer);
    let (registration, _proof, request) = registered(&writer.database);
    let mut preparation = writer
        .prepare_wal_reservation(&revision, Instant::now() + Duration::from_secs(2))
        .unwrap();
    let binding = preparation.capture_wal_binding(&registration).unwrap();
    preparation
        .retain_registered_wal_binding(&binding, &registration)
        .unwrap();
    assert_eq!(count(&request), 1);
    assert!(
        preparation
            .retain_registered_wal_binding(&binding, &registration)
            .is_err()
    );
    assert_eq!(count(&request), 1);
    assert!(other.execute_batch("BEGIN IMMEDIATE").is_err());
    let writer = preparation.abort().unwrap();
    assert_eq!(count(&request), 0);
    assert_eq!(writer.current_revision().unwrap(), revision);
    other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    drop(registration);
    writer.close().unwrap();
    assert_eq!(lease_state(&other), "released");
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
#[test]
fn denied_rollback_retains_count_until_original_draining_close() {
    let (_fixture, writer) = fixture();
    let revision = writer.current_revision().unwrap();
    let other = external(&writer);
    let (mut registration, proof, request) = registered(&writer.database);
    // SQLite destroys this function userdata while closing the original connection.
    struct ObserveClose {
        request: Arc<SourceAuthorityRequest>,
        seen: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Drop for ObserveClose {
        fn drop(&mut self) {
            self.seen.store(count(&self.request), Ordering::SeqCst);
        }
    }
    let seen = Arc::new(std::sync::atomic::AtomicUsize::new(usize::MAX));
    let observer = ObserveClose {
        request: Arc::clone(&request),
        seen: Arc::clone(&seen),
    };
    unsafe extern "C" fn invoke(
        ctx: *mut rusqlite::ffi::sqlite3_context,
        _: i32,
        _: *mut *mut rusqlite::ffi::sqlite3_value,
    ) {
        // SAFETY: SQLite supplies its live invocation context.
        unsafe {
            rusqlite::ffi::sqlite3_result_int(ctx, 0);
        }
    }
    unsafe extern "C" fn destroy(data: *mut std::ffi::c_void) {
        // SAFETY: SQLite owns exactly this Box after create_function_v2 and calls once
        // on registration failure or actual original connection close. No panic/SQL.
        unsafe {
            drop(Box::from_raw(data.cast::<ObserveClose>()));
        }
    }
    let deny = Arc::new(AtomicBool::new(false));
    let hook = Arc::clone(&deny);
    writer
        .database
        .connection
        .authorizer(Some(move |ctx: AuthContext<'_>| {
            if matches!(
                ctx.action,
                AuthAction::Transaction {
                    operation: TransactionOperation::Rollback
                }
            ) && hook.load(Ordering::SeqCst)
            {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }));
    let mut preparation = writer
        .prepare_wal_reservation(&revision, Instant::now() + Duration::from_secs(2))
        .unwrap();
    let binding = preparation.capture_wal_binding(&registration).unwrap();
    preparation
        .retain_registered_wal_binding(&binding, &registration)
        .unwrap();
    // SAFETY: original live connection, static terminated name, matching callbacks.
    // Ownership of the test-only userdata is transferred to SQLite, not a Rust wrapper.
    let rc = unsafe {
        rusqlite::ffi::sqlite3_create_function_v2(
            preparation.writer().database.connection.handle(),
            c"observe_registered_close".as_ptr(),
            0,
            rusqlite::ffi::SQLITE_UTF8,
            Box::into_raw(Box::new(observer)).cast(),
            Some(invoke),
            None,
            None,
            Some(destroy),
        )
    };
    assert_eq!(rc, rusqlite::ffi::SQLITE_OK);
    deny.store(true, Ordering::SeqCst);
    let failure = match preparation.abort() {
        Err(failure) => failure,
        Ok(_) => panic!("actual rollback denial must retain custody"),
    };
    assert_eq!(count(&request), 1);
    assert!(other.execute_batch("BEGIN IMMEDIATE").is_err());
    assert!(failure.source_authority_close_ready());
    request.begin_drain(proof.into_drain_marker()).unwrap();
    failure.close_for_retirement(&mut registration).unwrap();
    assert_eq!(seen.load(Ordering::SeqCst), 1);
    assert_eq!(count(&request), 0);
    assert_eq!(lease_state(&other), "released");
    other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
}

#[test]
fn typed_own_lease_reference_does_not_hide_external_snapshot() {
    let (_fixture, writer) = fixture();
    let other = external(&writer);
    let (mut registration, proof, request) = registered(&writer.database);
    let custody = registration
        .retain_registered_database_boundary(&writer.database)
        .unwrap();
    let snapshot_lease = Arc::clone(writer.database.source_lease.as_ref().unwrap());
    assert!(!custody.close_ready(&writer.database));
    request.begin_drain(proof.into_drain_marker()).unwrap();
    let result = registration
        .close_database_with_registered_custody(writer.into_catalog_database(), Some(custody));
    assert!(matches!(
        result,
        Err(SourceAuthorityError::Lease(
            SourceLeaseError::SnapshotsActive
        ))
    ));
    assert_eq!(count(&request), 0);
    assert_eq!(lease_state(&other), "active");
    drop(snapshot_lease);
    drop(registration);
    assert_eq!(lease_state(&other), "released");
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
#[test]
fn abandoned_registration_then_reservation_drop_closes_without_self_wait() {
    let (_fixture, writer) = fixture();
    let revision = writer.current_revision().unwrap();
    let other = external(&writer);
    let (registration, _proof, request) = registered(&writer.database);

    let mut preparation = writer
        .prepare_wal_reservation(&revision, Instant::now() + Duration::from_secs(2))
        .unwrap();
    let binding = preparation.capture_wal_binding(&registration).unwrap();
    preparation
        .retain_registered_wal_binding(&binding, &registration)
        .unwrap();
    drop(registration);
    assert_eq!(count(&request), 1);
    drop(preparation);
    assert_eq!(count(&request), 0);
    assert_eq!(lease_state(&other), "released");
    other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
#[test]
fn parked_validation_executes_no_ordinary_sql_and_drain_retains_custody() {
    let (_fixture, writer) = fixture();
    let revision = writer.current_revision().unwrap();
    let (registration, proof, request) = registered(&writer.database);
    let mut preparation = writer
        .prepare_wal_reservation(&revision, Instant::now() + Duration::from_secs(2))
        .unwrap();
    let binding = preparation.capture_wal_binding(&registration).unwrap();
    preparation
        .retain_registered_wal_binding(&binding, &registration)
        .unwrap();
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hook = Arc::clone(&attempts);
    preparation
        .writer()
        .database
        .connection
        .authorizer(Some(move |_: AuthContext<'_>| {
            hook.fetch_add(1, Ordering::SeqCst);
            Authorization::Deny
        }));
    for _ in 0..8 {
        preparation
            .validate_registered_wal_binding(&binding, &registration)
            .unwrap();
    }
    request.begin_drain(proof.into_drain_marker()).unwrap();
    assert!(
        preparation
            .validate_registered_wal_binding(&binding, &registration)
            .is_err()
    );
    assert_eq!(count(&request), 1);
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
    preparation
        .writer()
        .database
        .connection
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
    let writer = preparation.abort().unwrap();
    assert_eq!(count(&request), 0);
    drop(registration);
    writer.close().unwrap();
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
#[test]
fn foreign_and_expired_binding_cannot_acquire_retained_use() {
    let (fixture, writer) = fixture();
    let different = SourceDatabase::open_for_catalog_with_profile_guard(
        &fixture._temp.path().join("media"),
        &fixture._profile,
        Path::new("foreign-custody"),
    )
    .unwrap();
    let (foreign, _foreign_proof, foreign_request) = registered(&different);
    let revision = writer.current_revision().unwrap();
    let (registration, _proof, request) = registered(&writer.database);
    let mut preparation = writer
        .prepare_wal_reservation(&revision, Instant::now() + Duration::from_millis(200))
        .unwrap();
    let binding = preparation.capture_wal_binding(&registration).unwrap();
    assert!(
        preparation
            .retain_registered_wal_binding(&binding, &foreign)
            .is_err()
    );
    assert_eq!((count(&request), count(&foreign_request)), (0, 0));
    std::thread::sleep(Duration::from_millis(210));
    assert!(
        preparation
            .retain_registered_wal_binding(&binding, &registration)
            .is_err()
    );
    assert_eq!(count(&request), 0);
    let writer = preparation.abort().unwrap();
    drop(registration);
    drop(foreign);
    drop(different);
    writer.close().unwrap();
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
#[test]
fn failed_rollback_retry_releases_count_only_after_original_cleanup() {
    let (_fixture, writer) = fixture();
    let revision = writer.current_revision().unwrap();
    let (registration, _proof, request) = registered(&writer.database);
    let deny = Arc::new(AtomicBool::new(false));
    let hook = Arc::clone(&deny);
    writer
        .database
        .connection
        .authorizer(Some(move |ctx: AuthContext<'_>| {
            if matches!(
                ctx.action,
                AuthAction::Transaction {
                    operation: TransactionOperation::Rollback
                }
            ) && hook.load(Ordering::SeqCst)
            {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }));
    let mut preparation = writer
        .prepare_wal_reservation(&revision, Instant::now() + Duration::from_secs(2))
        .unwrap();
    let binding = preparation.capture_wal_binding(&registration).unwrap();
    preparation
        .retain_registered_wal_binding(&binding, &registration)
        .unwrap();
    deny.store(true, Ordering::SeqCst);
    let failure = match preparation.abort() {
        Err(f) => f,
        Ok(_) => panic!("real rollback must fail"),
    };
    assert_eq!(count(&request), 1);
    let failure = match failure.into_writer() {
        Err(f) => f,
        Ok(_) => panic!("denied retry must retain custody"),
    };
    assert_eq!(count(&request), 1);
    deny.store(false, Ordering::SeqCst);
    let writer = failure.into_writer().unwrap();
    assert_eq!(count(&request), 0);
    assert_eq!(writer.current_revision().unwrap(), revision);
    drop(registration);
    writer.close().unwrap();
}

#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "test-support"
))]
#[test]
fn timeout_restore_failure_keeps_count_after_rollback_until_confirmed_restore() {
    let (_fixture, mut writer) = fixture();
    let revision = writer.current_revision().unwrap();
    let other = external(&writer);
    let (registration, _proof, request) = registered(&writer.database);
    let timeout: u64 = writer
        .database
        .connection
        .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
        .unwrap();
    let fault = Arc::new(AtomicBool::new(false));
    writer.wal_timeout_restore_fault = Some(Arc::clone(&fault));
    let mut preparation = writer
        .prepare_wal_reservation(&revision, Instant::now() + Duration::from_secs(2))
        .unwrap();
    let binding = preparation.capture_wal_binding(&registration).unwrap();
    preparation
        .retain_registered_wal_binding(&binding, &registration)
        .unwrap();
    // Fault injection at the existing private pending-restoration boundary, after real
    // Registered acquisition. Normal preparation already restores this timeout.
    preparation.held.restore_timeout = Some(Duration::from_millis(timeout));
    fault.store(true, Ordering::SeqCst);
    let failure = match preparation.abort() {
        Err(f) => f,
        Ok(_) => panic!("actual restoration fault must retain custody"),
    };
    other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    assert_eq!(count(&request), 1);
    fault.store(false, Ordering::SeqCst);
    let writer = failure.into_writer().unwrap();
    assert_eq!(count(&request), 0);
    drop(registration);
    writer.close().unwrap();
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
#[test]
fn retained_reservation_wrong_guard_closes_original_without_retirement() {
    let (fixture, writer) = fixture();
    let other = external(&writer);
    let revision = writer.current_revision().unwrap();
    let (registration, _proof, request) = registered(&writer.database);
    let different = SourceDatabase::open_for_catalog_with_profile_guard(
        &fixture._temp.path().join("media"),
        &fixture._profile,
        Path::new("wrong-close-custody"),
    )
    .unwrap();
    let (mut wrong, proof, wrong_request) = registered(&different);
    wrong_request
        .begin_drain(proof.into_drain_marker())
        .unwrap();
    let mut preparation = writer
        .prepare_wal_reservation(&revision, Instant::now() + Duration::from_secs(2))
        .unwrap();
    let binding = preparation.capture_wal_binding(&registration).unwrap();
    preparation
        .retain_registered_wal_binding(&binding, &registration)
        .unwrap();
    assert!(matches!(
        preparation.close_for_retirement(&mut wrong),
        Err(SourceAuthorityError::GuardMismatch)
    ));
    assert_eq!(count(&request), 0);
    other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    // The original guard still retains the active lease. A wrong guard cannot seal its ack.
    assert_eq!(lease_state(&other), "active");
    drop(registration);
    assert_eq!(lease_state(&other), "released");
    wrong.close_database(different).unwrap();
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
#[test]
fn original_source_empty_mismatch_keeps_registered_count_until_actual_connection_close() {
    let (_fixture, writer) = fixture();
    let revision = writer.current_revision().unwrap();
    let other = external(&writer);
    let (registration, _proof, request) = registered(&writer.database);
    let mut preparation = writer
        .prepare_wal_reservation(&revision, Instant::now() + Duration::from_secs(2))
        .unwrap();
    let binding = preparation.capture_wal_binding(&registration).unwrap();
    preparation
        .retain_registered_wal_binding(&binding, &registration)
        .unwrap();
    assert_eq!(count(&request), 1);
    preparation
        .held
        .writer()
        .database
        .connection
        .execute(
            "INSERT INTO metadata(key,value) VALUES(?1,?2)",
            ["registered-empty-fault", "uncommitted"],
        )
        .unwrap();
    // Release itself must detect mismatch even without a preceding validation call.
    let failure = preparation
        .abort()
        .err()
        .expect("original changed custody stays private");
    assert!(matches!(
        failure.error(),
        SourceWalPreparationError::EmptyReservationChanged
    ));
    assert_eq!(count(&request), 1);
    assert!(failure.held.writer().database.connection.is_autocommit());
    let failure = failure
        .into_writer()
        .err()
        .expect("confirmed rollback cannot release original counted custody");
    assert_eq!(count(&request), 1);
    assert_eq!(
        other
            .query_row(
                "SELECT count(*) FROM metadata WHERE key=?1",
                ["registered-empty-fault"],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    drop(failure);
    assert_eq!(count(&request), 0);
    drop(registration);
    assert_eq!(lease_state(&other), "released");
}
