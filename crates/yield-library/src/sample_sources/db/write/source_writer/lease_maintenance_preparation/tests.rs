//! Actual profile-owned source/SQLite failure tests. No physical grant or journal fixture.
#![cfg(target_os = "linux")]
use super::*;
use crate::{
    app_dirs::{AppRootGuard, WritableProfileGuard},
    sample_sources::{DB_FILE_NAME, SourceDatabase, SourceId},
};
use rusqlite::{
    Connection, ErrorCode, OpenFlags,
    hooks::{AuthAction, AuthContext, Authorization, TransactionOperation},
};
use std::{
    fs,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
struct Fixture {
    _temp: tempfile::TempDir,
    _root: AppRootGuard,
    profile: WritableProfileGuard,
    media: std::path::PathBuf,
    path: std::path::PathBuf,
}
fn fixture() -> (Fixture, SourceWriter) {
    let temp = tempfile::tempdir().unwrap();
    let media = temp.path().join("media");
    fs::create_dir(&media).unwrap();
    let root = AppRootGuard::set(temp.path().join("profile")).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let path = profile.profile_root().join("source").join(DB_FILE_NAME);
    let writer = SourceWriter::new_for_scan(
        SourceDatabase::open_for_catalog_with_profile_guard(&media, &profile, Path::new("source"))
            .unwrap(),
        SourceId::from_string("source"),
        1,
    )
    .unwrap();
    (
        Fixture {
            _temp: temp,
            _root: root,
            profile,
            media,
            path,
        },
        writer,
    )
}
fn external(f: &Fixture) -> Connection {
    let other = Connection::open_with_flags(&f.path, OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    other.busy_timeout(Duration::ZERO).unwrap();
    other
}
fn prepare_owner(writer: SourceWriter) -> SourceLeaseMaintenancePreparation {
    let deadline = Instant::now() + Duration::from_secs(2);
    let observation = writer
        .capture_lease_maintenance_allocation(deadline)
        .unwrap();
    writer
        .prepare_lease_maintenance(observation, deadline)
        .unwrap()
}
fn settings(writer: &SourceWriter) -> Vec<u64> {
    [
        "max_page_count",
        "temp_store",
        "cache_spill",
        "wal_autocheckpoint",
        "busy_timeout",
    ]
    .iter()
    .map(|p| {
        writer
            .database
            .connection
            .pragma_query_value(None, p, |r| r.get(0))
            .unwrap()
    })
    .collect()
}
fn clear_authorizer(writer: &SourceWriter) {
    writer
        .database
        .connection
        .authorizer(Option::<fn(AuthContext<'_>) -> Authorization>::None);
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
#[test]
fn selected_settings_and_original_policy_are_exact_no_ordinary_or_control_changes() {
    let (f, writer) = fixture();
    for (p, v) in [
        ("temp_store", 0),
        ("cache_spill", 123),
        ("wal_autocheckpoint", 17),
        ("busy_timeout", 77),
    ] {
        writer
            .database
            .connection
            .pragma_update(None, p, v)
            .unwrap();
    }
    let original = settings(&writer);
    let ordinary = writer.database.connection.total_changes();
    let control = writer
        .database
        .source_lease_control_total_changes_for_tests()
        .unwrap()
        .unwrap();
    let other = external(&f);
    let before = lease(&other);
    let held = prepare_owner(writer);
    assert_eq!(settings(held.custody.writer()), vec![16384, 2, 0, 0, 0]);
    held.revalidate().unwrap();
    assert_eq!(
        held.custody.writer().database.connection.total_changes(),
        ordinary
    );
    assert_eq!(
        held.custody
            .writer()
            .database
            .source_lease_control_total_changes_for_tests()
            .unwrap()
            .unwrap(),
        control
    );
    assert_eq!(lease(&other), before);
    let writer = held.abort().unwrap();
    assert_eq!(settings(&writer), original);
    assert_eq!(writer.database.connection.total_changes(), ordinary);
    assert!(writer.current_revision().is_err());
    writer.close().unwrap();
}
#[test]
fn each_actual_policy_install_denial_retains_exact_writer_and_restores_partial_settings() {
    for name in [
        "max_page_count",
        "temp_store",
        "cache_spill",
        "wal_autocheckpoint",
    ] {
        let (_f, writer) = fixture();
        let original = settings(&writer);
        let deadline = Instant::now() + Duration::from_secs(2);
        let observation = writer
            .capture_lease_maintenance_allocation(deadline)
            .unwrap();
        let hit = Arc::new(AtomicBool::new(false));
        let captured = Arc::clone(&hit);
        writer.database.connection.authorizer(Some(move |ctx: AuthContext<'_>| {
            if matches!(ctx.action, AuthAction::Pragma { pragma_name, pragma_value: Some(_) } if pragma_name.eq_ignore_ascii_case(name)) {
                captured.store(true, Ordering::Release); Authorization::Deny
            } else { Authorization::Allow }
        }));
        let failure = writer
            .prepare_lease_maintenance(observation, deadline)
            .unwrap_err();
        assert!(
            hit.load(Ordering::Acquire),
            "actual install boundary {name}"
        );
        clear_authorizer(failure.custody.writer());
        let writer = failure.into_writer().unwrap();
        assert_eq!(
            settings(&writer),
            original,
            "partial install of {name} restored"
        );
        assert!(writer.database.connection.is_autocommit());
        writer.close().unwrap();
    }
}
#[test]
fn actual_rollback_denial_retains_lock_and_timeout_until_confirmed_retry() {
    let (f, writer) = fixture();
    let other = external(&f);
    let held = prepare_owner(writer);
    held.custody
        .writer()
        .database
        .connection
        .authorizer(Some(|ctx: AuthContext<'_>| {
            if matches!(
                ctx.action,
                AuthAction::Transaction {
                    operation: TransactionOperation::Rollback
                }
            ) {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }));
    let failure = held.abort().err().expect("cleanup must retain the writer");
    assert!(matches!(
        failure.error(),
        SourceLeaseMaintenancePreparationError::RollbackUnconfirmed
    ));
    assert_eq!(
        other
            .execute_batch("BEGIN IMMEDIATE")
            .unwrap_err()
            .sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy)
    );
    assert_eq!(settings(failure.custody.writer())[4], 0);
    clear_authorizer(failure.custody.writer());
    failure.into_writer().unwrap().close().unwrap();
}
#[test]
fn timeout_restoration_failure_retains_original_owner_until_explicit_retry() {
    let (_f, writer) = fixture();
    let original = settings(&writer);
    let mut held = prepare_owner(writer);
    held.custody.timeout_restore_fault = true;
    let mut failure = held.abort().err().expect("cleanup must retain the writer");
    assert!(matches!(
        failure.error(),
        SourceLeaseMaintenancePreparationError::Sqlite(_)
    ));
    assert!(failure.custody.writer().database.connection.is_autocommit());
    assert!(failure.custody.policy.is_some());
    failure.custody.timeout_restore_fault = false;
    let writer = failure.into_writer().unwrap();
    assert_eq!(settings(&writer), original);
    writer.close().unwrap();
}
#[test]
fn each_actual_settings_restore_denial_is_sticky_and_never_revives_owner() {
    for name in [
        "max_page_count",
        "temp_store",
        "cache_spill",
        "wal_autocheckpoint",
    ] {
        let (f, writer) = fixture();
        let other = external(&f);
        let held = prepare_owner(writer);
        held.custody.writer().database.connection.authorizer(Some(move |ctx: AuthContext<'_>| {
            if matches!(ctx.action, AuthAction::Pragma { pragma_name, pragma_value: Some(_) } if pragma_name.eq_ignore_ascii_case(name)) {
                Authorization::Deny
            } else { Authorization::Allow }
        }));
        let failure = held.abort().err().expect("cleanup must retain the writer");
        assert!(matches!(
            failure.error(),
            SourceLeaseMaintenancePreparationError::PolicyUnconfirmed
        ));
        assert!(failure.custody.writer().database.connection.is_autocommit());
        clear_authorizer(failure.custody.writer());
        let failure = failure
            .into_writer()
            .err()
            .expect("poisoned policy must retain the writer");
        assert!(matches!(
            failure.error(),
            SourceLeaseMaintenancePreparationError::PolicyUnconfirmed
        ));
        drop(failure);
        other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        assert_eq!(lease(&other).3, "released");
    }
}
#[test]
fn original_deadline_expires_under_contention_but_abort_confirms_cleanup_without_renewal() {
    let (f, writer) = fixture();
    let other = external(&f);
    let original = lease(&other);
    let deadline = Instant::now() + Duration::from_millis(250);
    let observation = writer
        .capture_lease_maintenance_allocation(deadline)
        .unwrap();
    let held = writer
        .prepare_lease_maintenance(observation, Instant::now() + Duration::from_secs(2))
        .unwrap();
    assert_eq!(held.deadline(), deadline);
    assert_eq!(
        other
            .execute_batch("BEGIN IMMEDIATE")
            .unwrap_err()
            .sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy)
    );
    std::thread::sleep(Duration::from_millis(260));
    assert!(matches!(
        held.revalidate(),
        Err(SourceLeaseMaintenancePreparationError::Deadline)
    ));
    let writer = held.abort().unwrap();
    assert_eq!(lease(&other), original);
    writer.close().unwrap();
}
#[test]
fn equal_tuple_moved_writer_and_other_source_observation_cannot_acquire_custody() {
    let (f, writer) = fixture();
    let deadline = Instant::now() + Duration::from_secs(2);
    let observation = writer
        .capture_lease_maintenance_allocation(deadline)
        .unwrap();
    let moved = SourceWriter::new_for_scan(
        writer.into_catalog_database(),
        SourceId::from_string("source"),
        1,
    )
    .unwrap();
    assert_eq!(
        moved.current_scan_fence().unwrap().lease_epoch(),
        observation.subject().lease_epoch()
    );
    let failure = moved
        .prepare_lease_maintenance(observation.clone(), deadline)
        .unwrap_err();
    assert!(matches!(
        failure.error(),
        SourceLeaseMaintenancePreparationError::Allocation(_)
    ));
    drop(failure);
    let other = SourceWriter::new_for_scan(
        SourceDatabase::open_for_catalog_with_profile_guard(
            &f.media,
            &f.profile,
            Path::new("foreign"),
        )
        .unwrap(),
        SourceId::from_string("foreign"),
        1,
    )
    .unwrap();
    assert!(
        other
            .prepare_lease_maintenance(observation, deadline)
            .is_err()
    );
}
#[test]
fn actual_root_loss_native_sidecar_substitution_and_unwind_never_release_locked_callable_owner() {
    let (f, writer) = fixture();
    let other = external(&f);
    let held = prepare_owner(writer);
    let shm = f.path.with_file_name(format!("{DB_FILE_NAME}-shm"));
    let parked = f.path.with_file_name("parked-shm");
    fs::rename(&shm, &parked).unwrap();
    fs::copy(&parked, &shm).unwrap();
    assert!(held.revalidate().is_err());
    let failure = held.abort().err().expect("cleanup must retain the writer");
    fs::remove_file(&shm).unwrap();
    fs::rename(&parked, &shm).unwrap();
    let writer = failure.into_writer().unwrap();
    let held = prepare_owner(writer);
    let parked_media = f.media.with_file_name("original-media");
    fs::rename(&f.media, &parked_media).unwrap();
    fs::create_dir(&f.media).unwrap();
    assert!(held.revalidate().is_err());
    let failure = held.abort().err().expect("cleanup must retain the writer");
    fs::remove_dir(&f.media).unwrap();
    fs::rename(&parked_media, &f.media).unwrap();
    let writer = failure.into_writer().unwrap();
    let held = prepare_owner(writer);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _held = held;
        panic!("actual owner unwind");
    }));
    assert!(outcome.is_err());
    other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    assert_eq!(lease(&other).3, "released");
}
#[test]
fn actual_snapshot_custody_blocks_close_readiness_without_fabricating_registration() {
    let (_f, writer) = fixture();
    // Genuine existing Prepared source authority, not fake REGISTERED phase or reserve grants.
    let authority = writer.database.prepare_source_authority().unwrap();
    let snapshot = writer.database.acquire_snapshot_write_fence().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let observation = writer
        .capture_lease_maintenance_allocation(deadline)
        .unwrap();
    let failure = writer
        .prepare_lease_maintenance(observation, deadline)
        .unwrap_err();
    assert!(!failure.source_authority_close_ready());
    drop(snapshot);
    assert!(failure.source_authority_close_ready());
    let writer = failure.into_writer().unwrap();
    drop(authority);
    writer.close().unwrap();
}
#[test]
fn preexisting_transaction_is_never_adopted_or_rolled_back() {
    let (_f, writer) = fixture();
    let deadline = Instant::now() + Duration::from_secs(2);
    let observation = writer
        .capture_lease_maintenance_allocation(deadline)
        .unwrap();
    writer
        .database
        .connection
        .execute_batch("BEGIN IMMEDIATE")
        .unwrap();
    let failure = writer
        .prepare_lease_maintenance(observation, deadline)
        .unwrap_err();
    assert!(matches!(
        failure.error(),
        SourceLeaseMaintenancePreparationError::TransactionActive
    ));
    assert!(!failure.custody.writer().database.connection.is_autocommit());
    assert!(failure.into_writer().is_err());
}
#[test]
fn actual_main_begin_denial_keeps_selected_policy_and_restores_all_original_settings() {
    use std::sync::atomic::AtomicUsize;
    let (_f, writer) = fixture();
    let original = settings(&writer);
    let deadline = Instant::now() + Duration::from_secs(2);
    let observation = writer
        .capture_lease_maintenance_allocation(deadline)
        .unwrap();
    let begins = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&begins);
    writer
        .database
        .connection
        .authorizer(Some(move |ctx: AuthContext<'_>| {
            if matches!(
                ctx.action,
                AuthAction::Transaction {
                    operation: TransactionOperation::Begin
                }
            ) {
                // Two finite audit/directory read snapshots before and after policy installation.
                // The fifth BEGIN is the actual sole nonblocking MAIN writer acquisition.
                if seen.fetch_add(1, Ordering::AcqRel) == 4 {
                    return Authorization::Deny;
                }
            }
            Authorization::Allow
        }));
    let failure = writer
        .prepare_lease_maintenance(observation, deadline)
        .unwrap_err();
    assert_eq!(begins.load(Ordering::Acquire), 5);
    assert!(matches!(
        failure.error(),
        SourceLeaseMaintenancePreparationError::Sqlite(_)
    ));
    assert!(failure.custody.policy.is_some());
    assert_eq!(settings(failure.custody.writer()), vec![16384, 2, 0, 0, 0]);
    clear_authorizer(failure.custody.writer());
    let writer = failure.into_writer().unwrap();
    assert_eq!(settings(&writer), original);
    writer.close().unwrap();
}
#[test]
fn actual_hard_wal_and_expiring_or_superseded_lease_reject_without_hidden_renewal() {
    {
        let (f, writer) = fixture();
        let wal = f.path.with_file_name(format!("{DB_FILE_NAME}-wal"));
        // Negative sparse-file fixture: actual retained VFS xFileSize, no injected scalar.
        let file = fs::OpenOptions::new().write(true).open(&wal).unwrap();
        let original = file.metadata().unwrap().len();
        file.set_len(crate::WAL_HARD_WATERMARK_BYTES).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let observation = writer
            .capture_lease_maintenance_allocation(deadline)
            .unwrap();
        let failure = writer
            .prepare_lease_maintenance(observation, deadline)
            .unwrap_err();
        assert!(matches!(
            failure.error(),
            SourceLeaseMaintenancePreparationError::Wal(WalAdmissionError::HardWatermark { .. })
        ));
        // Confirm cleanup against the original observed large object before restoring fixture.
        let writer = failure.into_writer().unwrap();
        file.set_len(original).unwrap();
        writer.close().unwrap();
    }
    for sql in [
        "UPDATE source_lease SET expires_at=unixepoch()+1",
        "UPDATE source_lease SET lease_epoch=lease_epoch+1",
    ] {
        let (f, writer) = fixture();
        let other = external(&f);
        let deadline = Instant::now() + Duration::from_secs(2);
        let observation = writer
            .capture_lease_maintenance_allocation(deadline)
            .unwrap();
        // Actual corrupted/near-expiry durable lease fixture, never a production lease grant.
        other.execute_batch(sql).unwrap();
        let changed = lease(&other);
        let failure = writer
            .prepare_lease_maintenance(observation, deadline)
            .unwrap_err();
        assert!(matches!(
            failure.error(),
            SourceLeaseMaintenancePreparationError::Allocation(_)
        ));
        assert_eq!(
            lease(&other),
            changed,
            "preparation must not renew or rewrite a lease"
        );
        assert!(failure.into_writer().is_err());
    }
}
#[test]
fn actual_unknown_native_wal_methods_reject_and_original_owner_remains_recoverable() {
    use rusqlite::ffi;
    let (_f, writer) = fixture();
    let deadline = Instant::now() + Duration::from_secs(2);
    let observation = writer
        .capture_lease_maintenance_allocation(deadline)
        .unwrap();
    let mut file: *mut ffi::sqlite3_file = std::ptr::null_mut();
    // SAFETY: this exclusive source owner remains alive throughout the fixture. SQLite returns
    // its borrowed actual WAL pointer. Never close, transfer or inspect an unknown layout.
    let result = unsafe {
        ffi::sqlite3_file_control(
            writer.database.connection.handle(),
            c"main".as_ptr(),
            ffi::SQLITE_FCNTL_JOURNAL_POINTER,
            (&mut file as *mut *mut ffi::sqlite3_file).cast(),
        )
    };
    assert_eq!(result, ffi::SQLITE_OK);
    assert!(!file.is_null());
    let original = unsafe { (*file).pMethods };
    assert!(!original.is_null());
    let copy = Box::new(unsafe { original.read() });
    // Unknown per-file methods deliberately fail the pinned own-wrapper identity check.
    unsafe {
        (*file).pMethods = &*copy;
    }
    let outcome = writer.prepare_lease_maintenance(observation, deadline);
    // Restore the same descriptor before any assertion, cleanup or method-box destruction.
    unsafe {
        (*file).pMethods = original;
    }
    let failure = outcome.unwrap_err();
    assert!(matches!(
        failure.error(),
        SourceLeaseMaintenancePreparationError::Allocation(LifecycleAllocationError::Unknown)
    ));
    failure.into_writer().unwrap().close().unwrap();
}
#[test]
fn actual_changed_held_pager_settings_and_unknown_schema_fail_readonly_revalidation() {
    let (_f, writer) = fixture();
    let held = prepare_owner(writer);
    held.custody
        .writer()
        .database
        .connection
        .pragma_update(None, "cache_spill", 123)
        .unwrap();
    assert!(matches!(
        held.revalidate(),
        Err(SourceLeaseMaintenancePreparationError::Owner(_))
    ));
    // Confirm original restoration after the injected settings corruption.
    let writer = held.abort().unwrap();
    let held = prepare_owner(writer);
    held.custody
        .writer()
        .database
        .connection
        .execute_batch("CREATE TEMP TABLE unknown_fixture(x)")
        .unwrap();
    assert!(held.revalidate().is_err());
    // The injected temporary object was created inside the held transaction. Confirmed
    // rollback removes it, so exact original policy restoration may safely recover the writer.
    let writer = held.abort().unwrap();
    assert_eq!(
        writer
            .database
            .connection
            .query_row("SELECT count(*) FROM sqlite_temp_master", [], |r| r
                .get::<_, u64>(0))
            .unwrap(),
        0
    );
    writer.close().unwrap();
}
#[test]
fn actual_timeout_readback_denial_prevents_false_restoration_and_requires_confirmed_retry() {
    let (_f, writer) = fixture();
    let original = settings(&writer);
    let held = prepare_owner(writer);
    held.custody.writer().database.connection.authorizer(Some(|ctx: AuthContext<'_>| {
        if matches!(ctx.action, AuthAction::Pragma { pragma_name, pragma_value: None } if pragma_name.eq_ignore_ascii_case("busy_timeout")) {
            Authorization::Deny
        } else { Authorization::Allow }
    }));
    let failure = held
        .abort()
        .err()
        .expect("actual readback must not claim restoration");
    assert!(matches!(
        failure.error(),
        SourceLeaseMaintenancePreparationError::Sqlite(_)
    ));
    assert!(failure.custody.restore_timeout.is_some());
    assert!(failure.custody.policy.is_some());
    clear_authorizer(failure.custody.writer());
    let writer = failure.into_writer().unwrap();
    assert_eq!(settings(&writer), original);
    writer.close().unwrap();
}
#[test]
fn expired_requested_deadline_and_reacquired_writer_never_create_a_preparation() {
    let (f, writer) = fixture();
    let deadline = Instant::now() + Duration::from_secs(2);
    let observation = writer
        .capture_lease_maintenance_allocation(deadline)
        .unwrap();
    let failure = writer
        .prepare_lease_maintenance(observation.clone(), Instant::now())
        .unwrap_err();
    assert!(matches!(
        failure.error(),
        SourceLeaseMaintenancePreparationError::Deadline
    ));
    let writer = failure.into_writer().unwrap();
    writer.close().unwrap();
    let reopened = SourceWriter::new_for_scan(
        SourceDatabase::open_for_catalog_with_profile_guard(
            &f.media,
            &f.profile,
            Path::new("source"),
        )
        .unwrap(),
        SourceId::from_string("source"),
        1,
    )
    .unwrap();
    let failure = reopened
        .prepare_lease_maintenance(observation, deadline)
        .unwrap_err();
    assert!(matches!(
        failure.error(),
        SourceLeaseMaintenancePreparationError::Allocation(_)
    ));
}

#[test]
fn genuine_binding_refuses_changed_lease_epoch_owner_or_state_in_retained_main() {
    // Deliberate private SQLite fault injection, not a production lease writer or permit.
    for sql in [
        "UPDATE source_lease SET lease_epoch=lease_epoch+1",
        "UPDATE source_lease SET owner_identity='foreign'",
        "UPDATE source_lease SET state='released',released_at=acquired_at",
        "UPDATE source_lease SET expires_at=acquired_at",
    ] {
        let (_f, writer) = fixture();
        let registration = writer.database.prepare_source_authority().unwrap();
        let held = prepare_owner(writer);
        let binding = held.capture_lease_binding(&registration).unwrap();
        held.custody
            .writer()
            .database
            .connection
            .execute_batch(sql)
            .unwrap();
        assert!(
            held.validate_lease_binding(&binding, &registration)
                .is_err()
        );
        assert!(held.capture_lease_binding(&registration).is_err());
        // Confirmed abort restores the real original rows, never authorizes the injected facts.
        let writer = held.abort().unwrap();
        drop(registration);
        writer.close().unwrap();
    }
}
