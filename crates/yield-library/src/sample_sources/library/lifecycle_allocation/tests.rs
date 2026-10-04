#![cfg(target_os = "linux")]
//! Actual SQLite/VFS storage fixtures only; none constructs genuine lifecycle receipts/permits.
use super::*;
use crate::app_dirs::{AppRootGuard, WritableProfileGuard};
use std::time::Duration;

fn fixture(test: impl FnOnce(&LibraryDatabase)) {
    let temp = tempfile::tempdir().unwrap();
    let _root = AppRootGuard::set(temp.path().to_path_buf()).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let database = LibraryDatabase::open_for_profile_guard(&profile, None).unwrap();
    test(&database);
}
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(2)
}
fn prepare(database: &LibraryDatabase) {
    database.connection.execute_batch("PRAGMA mmap_size=0; PRAGMA temp_store=MEMORY; PRAGMA cache_spill=0; PRAGMA wal_autocheckpoint=0; BEGIN IMMEDIATE").unwrap();
}
#[test]
fn actual_whole_schema_rejects_extra_objects_attached_temp_and_runtime_callbacks() {
    for mutation in [
        "CREATE TABLE unknown_table(value);",
        "CREATE INDEX unknown_index ON metadata(value);",
        "CREATE TRIGGER unknown_trigger AFTER INSERT ON metadata BEGIN SELECT 1; END;",
        "CREATE TEMP TABLE unknown_temp(value);",
        "ATTACH ':memory:' AS unknown_attached;",
    ] {
        fixture(|database| {
            database.connection.execute_batch(mutation).unwrap();
            prepare(database);
            let changes = database.connection.total_changes();
            assert_eq!(
                database.capture_lifecycle_allocation(deadline()),
                Err(LifecycleAllocationError::Unknown),
                "{mutation}"
            );
            assert_eq!(changes, database.connection.total_changes());
            database.connection.execute_batch("ROLLBACK").unwrap();
        });
    }
    fixture(|database| {
        unsafe extern "C" fn compare(
            _: *mut std::ffi::c_void,
            _: i32,
            _: *const std::ffi::c_void,
            _: i32,
            _: *const std::ffi::c_void,
        ) -> i32 {
            0
        }
        // SAFETY: actual exclusively borrowed connection retains the static callback until close.
        assert_eq!(
            unsafe {
                rusqlite::ffi::sqlite3_create_collation_v2(
                    database.connection.handle(),
                    c"unknown_lifecycle_callback".as_ptr(),
                    rusqlite::ffi::SQLITE_UTF8,
                    std::ptr::null_mut(),
                    Some(compare),
                    None,
                )
            },
            rusqlite::ffi::SQLITE_OK
        );
        prepare(database);
        assert_eq!(
            database.capture_lifecycle_allocation(deadline()),
            Err(LifecycleAllocationError::Unknown)
        );
        database.connection.execute_batch("ROLLBACK").unwrap();
    });
}
#[test]
fn actual_relation_receipt_and_active_caps_refuse_without_eviction() {
    fixture(|database| {
        // SQL shape/cardinality fixtures, not canonical authenticated subjects or receipts.
        database.connection.execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<8192) INSERT INTO source_lifecycle_references SELECT printf('%036d',x),printf('source%d',x),x'01','Retired',printf('%036d',x),'full-work',zeroblob(32) FROM n;
            WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<8192) INSERT INTO source_lifecycle_reference_receipts(operation_id,work_key,payload_identity,owner_identity,registration_id,disposition,receipt) SELECT printf('%036d',x),'full-work',zeroblob(32),printf('%036d',x),printf('%036d',x),'ExpectedRelationChanged',x'01' FROM n;
            UPDATE source_lifecycle_references SET state='Enrolled' WHERE CAST(registration_id AS INTEGER)<=4096;").unwrap();
        prepare(database);
        database.capture_lifecycle_allocation(deadline()).unwrap();
        database.connection.execute_batch("ROLLBACK; UPDATE source_lifecycle_references SET state='Draining' WHERE CAST(registration_id AS INTEGER)=4097;").unwrap();
        prepare(database);
        assert_eq!(
            database.capture_lifecycle_allocation(deadline()),
            Err(LifecycleAllocationError::Unknown)
        );
        database.connection.execute_batch("ROLLBACK; UPDATE source_lifecycle_references SET state='Retired' WHERE CAST(registration_id AS INTEGER)=4097; INSERT INTO source_lifecycle_references VALUES ('ffffffff-ffff-ffff-ffff-ffffffffffff','overflow',x'01','Retired','ffffffff-ffff-ffff-ffff-ffffffffffff','full-work',zeroblob(32));").unwrap();
        prepare(database);
        assert_eq!(
            database.capture_lifecycle_allocation(deadline()),
            Err(LifecycleAllocationError::Unknown)
        );
        database.connection.execute_batch("ROLLBACK; DELETE FROM source_lifecycle_references WHERE source_id='overflow'; INSERT INTO source_lifecycle_reference_receipts(operation_id,work_key,payload_identity,owner_identity,registration_id,disposition,receipt) VALUES('ffffffff-ffff-ffff-ffff-ffffffffffff','overflow',zeroblob(32),'ffffffff-ffff-ffff-ffff-ffffffffffff','ffffffff-ffff-ffff-ffff-ffffffffffff','ExpectedRelationChanged',x'01');").unwrap();
        prepare(database);
        assert_eq!(
            database.capture_lifecycle_allocation(deadline()),
            Err(LifecycleAllocationError::Unknown)
        );
        assert_eq!(counts(database), Err(LifecycleAllocationError::Unknown));
        database.connection.execute_batch("ROLLBACK").unwrap();
        assert_eq!(
            database
                .connection
                .query_row(
                    "SELECT count(*) FROM source_lifecycle_reference_receipts",
                    [],
                    |r| r.get::<_, u64>(0)
                )
                .unwrap(),
            8193
        );
    });
}
#[test]
fn actual_maximum_blob_workload_fits_explicit_logical_vfs_refusal_ceiling() {
    fixture(|database| {
        prepare(database);
        let observation = database.capture_lifecycle_allocation(deadline()).unwrap();
        database
            .connection
            .pragma_update(None, "max_page_count", observation.selected_maximum_pages())
            .unwrap();
        let attempt = database
            .connection
            .restrict(observation.logical_restriction().unwrap())
            .unwrap();
        // Real SQLite writes of bounded maximum blobs. These bytes are deliberately not a
        // command codec or genuine receipt; only VFS charging/restriction is under test.
        database.connection.execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<64) INSERT INTO source_lifecycle_references SELECT printf('%036d',x),printf('source%d',x),zeroblob(16384),'Retired',printf('%036d',x),'full-work',zeroblob(32) FROM n;
            WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<64) INSERT INTO source_lifecycle_reference_receipts(operation_id,work_key,payload_identity,owner_identity,registration_id,disposition,receipt) SELECT printf('%036d',x),'full-work',zeroblob(32),printf('%036d',x),printf('%036d',x),'ExpectedRelationChanged',zeroblob(16384) FROM n; COMMIT;").unwrap();
        let report = attempt.report();
        assert_eq!(report.failure, None);
        assert!(report.charged[1] > 2 * 1024 * 1024);
        for (charged, object) in
            report
                .charged
                .iter()
                .zip([observation.main(), observation.wal(), observation.shm()])
        {
            assert!(*charged <= object.write_ceiling());
        }
    });
}
#[test]
fn actual_vfs_refuses_growth_beyond_observed_logical_ceiling_before_native_write() {
    fixture(|database| {
        prepare(database);
        let observation = database.capture_lifecycle_allocation(deadline()).unwrap();
        let attempt = database
            .connection
            .restrict(observation.logical_restriction().unwrap())
            .unwrap();
        let before = crate::sqlite_wal::retained_wal_bytes(&database.connection).unwrap();
        let mut file: *mut rusqlite::ffi::sqlite3_file = std::ptr::null_mut();
        // SAFETY: actual associated live WAL callback borrowed only while its connection is held.
        unsafe {
            assert_eq!(
                rusqlite::ffi::sqlite3_file_control(
                    database.connection.handle(),
                    c"main".as_ptr(),
                    rusqlite::ffi::SQLITE_FCNTL_JOURNAL_POINTER,
                    (&mut file as *mut *mut rusqlite::ffi::sqlite3_file).cast()
                ),
                rusqlite::ffi::SQLITE_OK
            );
            assert!(!file.is_null());
            let byte = 1_u8;
            assert_ne!(
                (*(*file).pMethods).xWrite.unwrap()(
                    file,
                    (&byte as *const u8).cast(),
                    1,
                    i64::try_from(observation.wal().write_ceiling() + before).unwrap()
                ),
                rusqlite::ffi::SQLITE_OK
            );
        }
        assert_eq!(
            crate::sqlite_wal::retained_wal_bytes(&database.connection).unwrap(),
            before
        );
        assert_eq!(
            attempt.report().failure,
            Some(crate::sqlite_budget::BudgetFailure::Exhausted)
        );
        database.connection.execute_batch("ROLLBACK").unwrap();
    });
}
#[test]
fn actual_pager_limits_and_original_deadline_fail_closed_without_altering_maximum() {
    fixture(|database| {
        prepare(database);
        let first = database.capture_lifecycle_allocation(deadline()).unwrap();
        database
            .connection
            .pragma_update(None, "max_page_count", first.page_count())
            .unwrap();
        let reduced = database.capture_lifecycle_allocation(deadline()).unwrap();
        assert_eq!(reduced.selected_maximum_pages(), first.page_count());
        assert!(reduced.main().write_ceiling() < first.main().write_ceiling());
        assert_eq!(
            database.capture_lifecycle_allocation(Instant::now()),
            Err(LifecycleAllocationError::Budget)
        );
        assert_eq!(
            database.capture_lifecycle_allocation(Instant::now() + Duration::from_secs(3)),
            Err(LifecycleAllocationError::Budget)
        );
        database.connection.execute_batch("ROLLBACK").unwrap();
        assert_eq!(
            database
                .connection
                .pragma_query_value(None, "max_page_count", |r| r.get::<_, u64>(0))
                .unwrap(),
            first.page_count()
        );
    });
}
