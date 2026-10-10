//! Actual never-audited source/SQLite lifecycle tests; no capacity provider is installed.
#![cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
#[path = "registration_generation_tests.rs"]
mod registration_generation_tests;
#[path = "retirement_policy/registered_tests.rs"]
mod retirement_policy_tests;
use super::*;
use crate::{
    app_dirs::{AppRootGuard, WritableProfileGuard},
    sample_sources::{
        SourceDatabase, SourceId,
        db::{
            SourceAuthorityError, SourceAuthorityRegistration,
            source_authority::{SourceAuthorityRequest, SourceAuthorityUseProof},
            source_lease::SourceLeaseError,
        },
    },
};
use rusqlite::{
    Connection, ErrorCode,
    hooks::{AuthAction, AuthContext, Authorization, TransactionOperation},
};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
struct Fixture {
    _temp: tempfile::TempDir,
    _root: AppRootGuard,
    profile: WritableProfileGuard,
    media: std::path::PathBuf,
    _runtime: crate::test_runtime::TestRuntimeGuard,
}
fn fixture() -> (Fixture, SourceWriter) {
    let runtime = crate::test_runtime::TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    // macOS /var is a symlink; genuine no-follow profile acquisition requires
    // the original canonical temporary namespace, not relaxed ancestor checks.
    let original_root = std::fs::canonicalize(temp.path()).unwrap();
    let media = original_root.join("media");
    std::fs::create_dir(&media).unwrap();
    let root = AppRootGuard::set(original_root.join("profile")).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        &media,
        &profile,
        Path::new("registered-lease-custody"),
    )
    .unwrap();
    let writer = SourceWriter::new_for_scan(
        database,
        SourceId::from_string("registered-lease-custody"),
        1,
    )
    .unwrap();
    assert!(writer.current_revision().is_err());
    (
        Fixture {
            _temp: temp,
            _root: root,
            profile,
            media,
            _runtime: runtime,
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
    // Only the actual confirmation lifecycle; no journal/physical admission is asserted.
    let envelope = registration.journal_request().unwrap();
    let proof = envelope
        .request
        .accept_confirmation(envelope.confirmation)
        .unwrap();
    (registration, proof, envelope.request)
}
fn count(request: &SourceAuthorityRequest) -> usize {
    request.registered_uses_for_tests()
}
fn prepare(writer: SourceWriter) -> SourceLeaseMaintenancePreparation {
    let deadline = Instant::now() + Duration::from_secs(2);
    let input = writer
        .capture_lease_maintenance_allocation(deadline)
        .unwrap();
    writer.prepare_lease_maintenance(input, deadline).unwrap()
}
fn retain(
    held: &mut SourceLeaseMaintenancePreparation,
    registration: &SourceAuthorityRegistration,
) -> SourceLeaseMaintenanceBinding {
    let binding = held.capture_lease_binding(registration).unwrap();
    held.retain_registered_lease_binding(&binding, registration)
        .unwrap();
    binding
}
fn external(writer: &SourceWriter) -> Connection {
    let other = Connection::open(&writer.database.db_path).unwrap();
    other.busy_timeout(Duration::ZERO).unwrap();
    other
}
fn lease(other: &Connection) -> (String, i64, i64, String) {
    other
        .query_row(
            "SELECT owner_identity,lease_epoch,expires_at,state FROM source_lease",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap()
}
fn busy(other: &Connection) {
    assert_eq!(
        other
            .execute_batch("BEGIN IMMEDIATE")
            .unwrap_err()
            .sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy)
    );
}
fn settings(writer: &SourceWriter) -> Vec<u64> {
    [
        "max_page_count",
        "temp_store",
        "cache_spill",
        "wal_autocheckpoint",
        "busy_timeout",
    ]
    .into_iter()
    .map(|p| {
        writer
            .database
            .connection
            .pragma_query_value(None, p, |r| r.get(0))
            .unwrap()
    })
    .collect()
}
fn clear(writer: &SourceWriter) {
    writer
        .database
        .connection
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
}
fn rollback_denial(writer: &SourceWriter) -> Arc<AtomicBool> {
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
    deny
}
// Installed only after real capture/retention: the metadata guard must reject unknown
// functions at admission. SQLite destroys this userdata on actual original close.
fn observe_close(writer: &SourceWriter, request: &Arc<SourceAuthorityRequest>) -> Arc<AtomicUsize> {
    struct Observe {
        request: Arc<SourceAuthorityRequest>,
        seen: Arc<AtomicUsize>,
    }
    impl Drop for Observe {
        fn drop(&mut self) {
            self.seen.store(count(&self.request), Ordering::SeqCst);
        }
    }
    unsafe extern "C" fn invoke(
        ctx: *mut rusqlite::ffi::sqlite3_context,
        _: i32,
        _: *mut *mut rusqlite::ffi::sqlite3_value,
    ) {
        // SAFETY: live invocation context supplied by SQLite.
        unsafe {
            rusqlite::ffi::sqlite3_result_int(ctx, 0);
        }
    }
    unsafe extern "C" fn destroy(data: *mut std::ffi::c_void) {
        // SAFETY: SQLite owns this Box and destroys it once on failure or original close.
        unsafe {
            drop(Box::from_raw(data.cast::<Observe>()));
        }
    }
    let seen = Arc::new(AtomicUsize::new(usize::MAX));
    let observer = Observe {
        request: Arc::clone(request),
        seen: Arc::clone(&seen),
    };
    // SAFETY: live exclusive original connection, static terminated name, matching callbacks;
    // ownership of userdata transfers to SQLite even if registration fails.
    let rc = unsafe {
        rusqlite::ffi::sqlite3_create_function_v2(
            writer.database.connection.handle(),
            c"observe_registered_lease_close".as_ptr(),
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
    seen
}

#[test]
fn preaudit_prepared_refuses_and_registered_retention_releases_after_exact_cleanup() {
    let (_fixture, writer) = fixture();
    let original = settings(&writer);
    let ordinary = writer.database.connection.total_changes();
    let other = external(&writer);
    let before = lease(&other);
    let registration = writer.database.prepare_source_authority().unwrap();
    let envelope = registration.journal_request().unwrap();
    let request = envelope.request;
    let mut held = prepare(writer);
    let prepared = held.capture_lease_binding(&registration).unwrap();
    assert!(
        held.retain_registered_lease_binding(&prepared, &registration)
            .is_err()
    );
    assert_eq!(count(&request), 0);
    let _proof = request.accept_confirmation(envelope.confirmation).unwrap();
    assert!(
        held.retain_registered_lease_binding(&prepared, &registration)
            .is_err()
    );
    let binding = retain(&mut held, &registration);
    assert_eq!(count(&request), 1);
    assert!(
        held.retain_registered_lease_binding(&binding, &registration)
            .is_err()
    );
    assert_eq!(count(&request), 1);
    assert!(held.source_authority_close_ready());
    busy(&other);
    let writer = held.abort().unwrap();
    assert_eq!(count(&request), 0);
    assert_eq!(settings(&writer), original);
    assert_eq!(writer.database.connection.total_changes(), ordinary);
    assert_eq!(lease(&other), before);
    assert!(writer.current_revision().is_err());
    other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    drop(registration);
    writer.close().unwrap();
}
#[test]
fn preaudit_registered_parked_validation_has_no_ordinary_sql_and_drain_keeps_count() {
    let (_fixture, writer) = fixture();
    let (registration, proof, request) = registered(&writer.database);
    let mut held = prepare(writer);
    let binding = retain(&mut held, &registration);
    let attempts = Arc::new(AtomicUsize::new(0));
    let hook = Arc::clone(&attempts);
    held.custody
        .writer()
        .database
        .connection
        .authorizer(Some(move |ctx: AuthContext<'_>| {
            // Existing policy/WAL checks require only these value-free setting/mode reads.
            if matches!(ctx.action, AuthAction::Pragma { pragma_name, pragma_value: None }
                if matches!(pragma_name, "max_page_count" | "temp_store" | "cache_spill" | "wal_autocheckpoint" | "journal_mode")) {
                Authorization::Allow
            } else {
                hook.fetch_add(1, Ordering::SeqCst);
                Authorization::Deny
            }
        }));
    for _ in 0..8 {
        held.validate_registered_lease_binding(&binding, &registration)
            .unwrap();
    }
    request.begin_drain(proof.into_drain_marker()).unwrap();
    assert!(
        held.validate_registered_lease_binding(&binding, &registration)
            .is_err()
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
    assert_eq!(count(&request), 1);
    clear(held.custody.writer());
    let writer = held.abort().unwrap();
    assert_eq!(count(&request), 0);
    assert!(writer.current_revision().is_err());
    drop(registration);
    writer.close().unwrap();
}
#[test]
fn preaudit_denied_rollback_retry_retains_registered_count_and_original_policy() {
    let (_fixture, writer) = fixture();
    let original = settings(&writer);
    let other = external(&writer);
    let (registration, _proof, request) = registered(&writer.database);
    let deny = rollback_denial(&writer);
    let mut held = prepare(writer);
    retain(&mut held, &registration);
    deny.store(true, Ordering::SeqCst);
    let failure = held.abort().err().expect("real rollback denial");
    assert_eq!(count(&request), 1);
    busy(&other);
    let failure = failure.into_writer().err().expect("still denied");
    assert_eq!(count(&request), 1);
    deny.store(false, Ordering::SeqCst);
    let writer = failure.into_writer().unwrap();
    assert_eq!(count(&request), 0);
    assert_eq!(settings(&writer), original);
    assert!(writer.current_revision().is_err());
    drop(registration);
    writer.close().unwrap();
}
#[test]
fn preaudit_timeout_restore_fault_keeps_registered_count_until_real_restoration() {
    let (_fixture, writer) = fixture();
    let original = settings(&writer);
    let other = external(&writer);
    let (registration, _proof, request) = registered(&writer.database);
    let mut held = prepare(writer);
    retain(&mut held, &registration);
    assert!(held.custody.restore_timeout.is_some());
    held.custody.timeout_restore_fault = true;
    let mut failure = held
        .abort()
        .err()
        .expect("actual pending timeout restoration fault");
    assert_eq!(count(&request), 1);
    assert!(failure.custody.policy.is_some());
    other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    failure.custody.timeout_restore_fault = false;
    let writer = failure.into_writer().unwrap();
    assert_eq!(count(&request), 0);
    assert_eq!(settings(&writer), original);
    drop(registration);
    writer.close().unwrap();
}
#[test]
fn preaudit_each_sticky_policy_failure_holds_count_until_actual_connection_close() {
    for name in [
        "max_page_count",
        "temp_store",
        "cache_spill",
        "wal_autocheckpoint",
    ] {
        let (_fixture, writer) = fixture();
        let other = external(&writer);
        let (registration, _proof, request) = registered(&writer.database);
        let mut held = prepare(writer);
        retain(&mut held, &registration);
        held.custody.writer().database.connection.authorizer(Some(move |ctx: AuthContext<'_>| {
            if matches!(ctx.action,AuthAction::Pragma { pragma_name,pragma_value:Some(_) } if pragma_name.eq_ignore_ascii_case(name)) { Authorization::Deny } else { Authorization::Allow }
        }));
        let failure = held
            .abort()
            .err()
            .expect("real original policy restoration denial");
        assert!(matches!(
            failure.error(),
            SourceLeaseMaintenancePreparationError::PolicyUnconfirmed
        ));
        assert_eq!(count(&request), 1);
        clear(failure.custody.writer());
        let failure = failure
            .into_writer()
            .err()
            .expect("sticky failure must not revive owner");
        assert_eq!(count(&request), 1);
        let seen = observe_close(failure.custody.writer(), &request);
        drop(failure);
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        assert_eq!(count(&request), 0);
        assert_eq!(lease(&other).3, "active");
        drop(registration);
        assert_eq!(lease(&other).3, "released");
    }
}
#[test]
fn preaudit_abandoned_registration_close_without_reuse_does_not_wait_on_own_count() {
    let (_fixture, writer) = fixture();
    let other = external(&writer);
    let (registration, _proof, request) = registered(&writer.database);
    let mut held = prepare(writer);
    retain(&mut held, &registration);
    let seen = observe_close(held.custody.writer(), &request);
    drop(registration);
    assert_eq!(count(&request), 1);
    held.close_without_reuse();
    assert_eq!(seen.load(Ordering::SeqCst), 1);
    assert_eq!(count(&request), 0);
    assert_eq!(lease(&other).3, "released");
    other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
}
#[test]
fn preaudit_original_draining_close_releases_count_after_sqlite_before_lease_ack() {
    let (_fixture, writer) = fixture();
    let other = external(&writer);
    let (mut registration, proof, request) = registered(&writer.database);
    let deny = rollback_denial(&writer);
    let mut held = prepare(writer);
    retain(&mut held, &registration);
    let seen = observe_close(held.custody.writer(), &request);
    deny.store(true, Ordering::SeqCst);
    let failure = held
        .abort()
        .err()
        .expect("denied rollback retains native owner");
    assert!(failure.source_authority_close_ready());
    assert_eq!(count(&request), 1);
    request.begin_drain(proof.into_drain_marker()).unwrap();
    failure.close_for_retirement(&mut registration).unwrap();
    assert_eq!(seen.load(Ordering::SeqCst), 1);
    assert_eq!(count(&request), 0);
    assert_eq!(lease(&other).3, "released");
    other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
}
#[test]
fn preaudit_external_retained_lease_reference_blocks_retirement_without_own_count_waiver() {
    let (_fixture, writer) = fixture();
    let other = external(&writer);
    let (mut registration, proof, request) = registered(&writer.database);
    let deny = rollback_denial(&writer);
    let mut held = prepare(writer);
    retain(&mut held, &registration);
    let external_lease = Arc::clone(
        held.custody
            .writer()
            .database
            .source_lease
            .as_ref()
            .unwrap(),
    );
    assert!(!held.source_authority_close_ready());
    deny.store(true, Ordering::SeqCst);
    let failure = held.abort().err().expect("denied rollback");
    assert!(!failure.source_authority_close_ready());
    request.begin_drain(proof.into_drain_marker()).unwrap();
    assert!(matches!(
        failure.close_for_retirement(&mut registration),
        Err(SourceAuthorityError::Lease(
            SourceLeaseError::SnapshotsActive
        ))
    ));
    assert_eq!(count(&request), 0);
    assert_eq!(lease(&other).3, "active");
    drop(external_lease);
    drop(registration);
    assert_eq!(lease(&other).3, "released");
}
#[test]
fn preaudit_foreign_registration_and_expired_binding_cannot_retain_use() {
    let (fixture, writer) = fixture();
    let different = SourceDatabase::open_for_catalog_with_profile_guard(
        &fixture.media,
        &fixture.profile,
        Path::new("foreign-lease-custody"),
    )
    .unwrap();
    let (foreign, _proof, foreign_request) = registered(&different);
    let (registration, _proof, request) = registered(&writer.database);
    let deadline = Instant::now() + Duration::from_millis(250);
    let input = writer
        .capture_lease_maintenance_allocation(deadline)
        .unwrap();
    let mut held = writer.prepare_lease_maintenance(input, deadline).unwrap();
    let binding = held.capture_lease_binding(&registration).unwrap();
    assert!(
        held.retain_registered_lease_binding(&binding, &foreign)
            .is_err()
    );
    assert_eq!((count(&request), count(&foreign_request)), (0, 0));
    std::thread::sleep(Duration::from_millis(260));
    assert!(
        held.retain_registered_lease_binding(&binding, &registration)
            .is_err()
    );
    assert_eq!(count(&request), 0);
    let writer = held.abort().unwrap();
    drop(registration);
    drop(foreign);
    drop(different);
    writer.close().unwrap();
}
#[test]
fn preaudit_wrong_draining_guard_closes_original_without_returning_retirement_ack() {
    let (fixture, writer) = fixture();
    let other = external(&writer);
    let (registration, _proof, request) = registered(&writer.database);
    let different = SourceDatabase::open_for_catalog_with_profile_guard(
        &fixture.media,
        &fixture.profile,
        Path::new("wrong-drain-lease"),
    )
    .unwrap();
    let (mut wrong, proof, wrong_request) = registered(&different);
    wrong_request
        .begin_drain(proof.into_drain_marker())
        .unwrap();
    let deny = rollback_denial(&writer);
    let mut held = prepare(writer);
    retain(&mut held, &registration);
    deny.store(true, Ordering::SeqCst);
    let failure = held.abort().err().expect("denied rollback");
    assert!(matches!(
        failure.close_for_retirement(&mut wrong),
        Err(SourceAuthorityError::GuardMismatch)
    ));
    assert_eq!(count(&request), 0);
    assert_eq!(lease(&other).3, "active");
    drop(registration);
    assert_eq!(lease(&other).3, "released");
    wrong.close_database(different).unwrap();
}
