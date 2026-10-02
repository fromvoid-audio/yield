use super::*;
use cap_std::fs::Dir;
use rusqlite::OpenFlags;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;

struct Fixture {
    connection: OwnedConnection,
    _root: Dir,
    _temp: tempfile::TempDir,
}
fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let root = Dir::open_ambient_dir(temp.path(), cap_std::ambient_authority()).unwrap();
    #[cfg(target_os = "linux")]
    let path = std::path::PathBuf::from(format!("/proc/self/fd/{}/test.db", root.as_raw_fd()));
    #[cfg(not(target_os = "linux"))]
    let path = temp.path().canonicalize().unwrap().join("test.db");
    let connection = open_owned(
        path,
        OpenFlags::default() | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        None,
    )
    .unwrap();
    connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA temp_store=MEMORY; PRAGMA mmap_size=0; PRAGMA wal_autocheckpoint=0; CREATE TABLE data(value BLOB); PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
    Fixture {
        connection,
        _root: root,
        _temp: temp,
    }
}
fn budget(db: u64, wal: u64, shm: u64) -> LogicalBudget {
    LogicalBudget::new([db, wal, shm], 4096, 4096, 4096).unwrap()
}
fn file(connection: &Connection, op: c_int) -> *mut ffi::sqlite3_file {
    let mut f: *mut ffi::sqlite3_file = ptr::null_mut();
    // SAFETY: sole owning thread, borrowed only while the connection remains alive.
    assert_eq!(
        unsafe {
            ffi::sqlite3_file_control(
                connection.handle(),
                c"main".as_ptr(),
                op,
                (&mut f as *mut *mut ffi::sqlite3_file).cast(),
            )
        },
        ffi::SQLITE_OK
    );
    assert!(!f.is_null());
    f
}
fn size(f: *mut ffi::sqlite3_file) -> i64 {
    let mut value = -1;
    // SAFETY: tests retain the owning connection throughout this borrowed descriptor call.
    assert_eq!(
        unsafe { (*(*f).pMethods).xFileSize.unwrap()(f, &mut value) },
        ffi::SQLITE_OK
    );
    value
}
#[test]
fn actual_wal_commit_and_retained_size_delegate_under_one_budget() {
    let f = fixture();
    let c = &f.connection;
    c.execute_batch("BEGIN IMMEDIATE").unwrap();
    let attempt = c
        .restrict(budget(1024 * 1024, 1024 * 1024, 1024 * 1024))
        .unwrap();
    c.execute_batch("INSERT INTO data VALUES(zeroblob(8192)); COMMIT")
        .unwrap();
    let report = attempt.report();
    assert!(report.charged[1] >= 8192);
    assert!(report.charged[2] >= 32768);
    assert_eq!(report.failure, None);
    assert!(crate::sqlite_wal::retained_wal_bytes(c).unwrap() > 0);
    assert!(crate::sqlite_wal::retained_database_bytes(c).unwrap() >= 4096);
    drop(attempt);
    assert_eq!(
        c.query_row("SELECT length(value) FROM data", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        8192
    );
    c.execute_batch("BEGIN IMMEDIATE; INSERT INTO data VALUES(1)")
        .unwrap();
    assert!(c.execute_batch("COMMIT").is_err()); // no refill after scope Drop
}
#[test]
fn actual_overspend_rejects_before_native_write_and_retains_charge() {
    let f = fixture();
    let c = &f.connection;
    c.execute_batch("BEGIN IMMEDIATE").unwrap();
    let wal = file(c, ffi::SQLITE_FCNTL_JOURNAL_POINTER);
    let before = size(wal);
    let attempt = c.restrict(budget(0, 0, 1024 * 1024)).unwrap();
    assert!(
        c.execute_batch("INSERT INTO data VALUES(42); COMMIT")
            .is_err()
    );
    assert_eq!(size(wal), before);
    assert_eq!(attempt.report().failure, Some(BudgetFailure::Exhausted));
    assert_eq!(attempt.report().charged[1], 0);
    c.execute_batch("ROLLBACK").ok();
    assert_eq!(
        c.query_row("SELECT count(*) FROM data", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    drop(attempt);
    c.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert!(c.restrict(budget(1 << 20, 1 << 20, 1 << 20)).is_err());
}
#[test]
fn actual_cache_spill_cannot_escape_wal_ceiling() {
    let f = fixture();
    let c = &f.connection;
    c.execute_batch("PRAGMA cache_size=1; PRAGMA cache_spill=ON; BEGIN IMMEDIATE")
        .unwrap();
    let attempt = c.restrict(budget(0, 65536, 1024 * 1024)).unwrap();
    // A many-page statement spills through the real pager BEFORE COMMIT.
    assert!(c.execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<100) INSERT INTO data SELECT zeroblob(4096) FROM n").is_err());
    let report = attempt.report();
    assert_eq!(report.failure, Some(BudgetFailure::Exhausted));
    assert!(report.charged[1] > 0 && report.charged[1] <= 65536);
    c.execute_batch("ROLLBACK").ok();
    assert_eq!(attempt.report().charged, report.charged);
}
#[test]
fn native_partial_write_keeps_full_charge_and_rejects_retry() {
    let f = fixture();
    let c = &f.connection;
    c.execute_batch("BEGIN IMMEDIATE").unwrap();
    let wal = file(c, ffi::SQLITE_FCNTL_JOURNAL_POINTER);
    let attempt = c.restrict(budget(1 << 20, 1 << 20, 1 << 20)).unwrap();
    *c.registration
        .as_ref()
        .unwrap()
        .context
        .partial_write
        .lock()
        .unwrap() = Some(8);
    assert!(
        c.execute_batch("INSERT INTO data VALUES(42); COMMIT")
            .is_err()
    );
    let report = attempt.report();
    assert_eq!(report.failure, Some(BudgetFailure::NativeIo));
    assert!(report.charged[1] >= 12288);
    assert!(size(wal) >= 8);
    c.execute_batch("ROLLBACK").ok();
    assert_eq!(attempt.report().charged, report.charged);
    // SAFETY: real retained WAL object; a rejected pre-forward call cannot change its length.
    let before = size(wal);
    let bytes = [0u8; 4096];
    assert_ne!(
        unsafe { (*(*wal).pMethods).xWrite.unwrap()(wal, bytes.as_ptr().cast(), 4096, before) },
        ffi::SQLITE_OK
    );
    assert_eq!(size(wal), before);
}
#[test]
fn real_shm_growth_size_hint_truncate_and_unknown_controls_are_guarded() {
    let f = fixture();
    let c = &f.connection;
    c.execute_batch("BEGIN IMMEDIATE").unwrap();
    let main = file(c, ffi::SQLITE_FCNTL_FILE_POINTER);
    let wal = file(c, ffi::SQLITE_FCNTL_JOURNAL_POINTER);
    let attempt = c.restrict(budget(0, 0, 40960)).unwrap();
    let mut mapped = ptr::null_mut();
    // SAFETY: live main file and real SHM mapping callback, attempted growth exceeds remaining ceiling.
    assert_eq!(
        unsafe { (*(*main).pMethods).xShmMap.unwrap()(main, 1, 32768, 1, &mut mapped) },
        ffi::SQLITE_FULL
    );
    assert!(mapped.is_null());
    assert_eq!(attempt.report().failure, Some(BudgetFailure::Exhausted));
    let before = size(wal);
    assert_ne!(
        unsafe { (*(*wal).pMethods).xTruncate.unwrap()(wal, before + 4096) },
        ffi::SQLITE_OK
    );
    assert_eq!(size(wal), before);
    drop(attempt);
    let f = fixture();
    let c = &f.connection;
    c.execute_batch("BEGIN IMMEDIATE").unwrap();
    let main = file(c, ffi::SQLITE_FCNTL_FILE_POINTER);
    let attempt = c.restrict(budget(0, 0, 1 << 20)).unwrap();
    let before = size(main);
    let mut hint = before + 4096;
    assert_eq!(
        unsafe {
            (*(*main).pMethods).xFileControl.unwrap()(
                main,
                ffi::SQLITE_FCNTL_SIZE_HINT,
                (&mut hint as *mut i64).cast(),
            )
        },
        ffi::SQLITE_FULL
    );
    assert_eq!(size(main), before);
    drop(attempt);
    let f = fixture();
    let c = &f.connection;
    c.execute_batch("BEGIN IMMEDIATE").unwrap();
    let main = file(c, ffi::SQLITE_FCNTL_FILE_POINTER);
    let attempt = c.restrict(budget(1 << 20, 1 << 20, 1 << 20)).unwrap();
    assert_ne!(
        unsafe { (*(*main).pMethods).xFileControl.unwrap()(main, 9999, ptr::null_mut()) },
        ffi::SQLITE_OK
    );
    assert_eq!(
        attempt.report().failure,
        Some(BudgetFailure::UnknownCoverage)
    );
}
#[test]
fn private_vfs_rejects_second_main_alias_and_unplanned_file_role() {
    let f = fixture();
    let c = &f.connection;
    let alias = f._temp.path().join("alias.db");
    std::fs::hard_link(f._temp.path().join("test.db"), &alias).unwrap();
    assert!(
        c.execute("ATTACH DATABASE ?1 AS alias", [alias.to_str().unwrap()])
            .is_err()
    );
    c.execute_batch("BEGIN IMMEDIATE").unwrap();
    let attempt = c.restrict(budget(1 << 20, 1 << 20, 1 << 20)).unwrap();
    let r = c.registration.as_ref().unwrap();
    let layout = std::alloc::Layout::from_size_align(r.vfs.szOsFile as usize, 8).unwrap();
    // SAFETY: SQLite-sized/aligned zeroed output; TEMP_DB has no special SQLite filename trailer.
    let native = unsafe { std::alloc::alloc_zeroed(layout) }.cast::<ffi::sqlite3_file>();
    assert!(!native.is_null());
    let name = CString::new(f._temp.path().join("extra.db").to_str().unwrap()).unwrap();
    let mut out = 0;
    let rc = unsafe {
        r.vfs.xOpen.unwrap()(
            (&*r.vfs as *const ffi::sqlite3_vfs).cast_mut(),
            name.as_ptr(),
            native,
            ffi::SQLITE_OPEN_TEMP_DB | ffi::SQLITE_OPEN_READWRITE | ffi::SQLITE_OPEN_CREATE,
            &mut out,
        )
    };
    assert_ne!(rc, ffi::SQLITE_OK);
    assert!(!f._temp.path().join("extra.db").exists());
    assert_eq!(
        attempt.report().failure,
        Some(BudgetFailure::UnknownCoverage)
    );
    unsafe { std::alloc::dealloc(native.cast(), layout) };
}
#[test]
fn correct_drop_order_failed_close_and_legacy_extraction() {
    let f = fixture();
    let c = f.connection;
    let name = c.registration.as_ref().unwrap().name.clone();
    assert!(!unsafe { ffi::sqlite3_vfs_find(name.as_ptr()) }.is_null());
    let mut stmt = ptr::null_mut(); // real unfinalized C statement forces SQLITE_BUSY close
    assert_eq!(
        unsafe {
            ffi::sqlite3_prepare_v2(
                c.handle(),
                c"SELECT 1".as_ptr(),
                -1,
                &mut stmt,
                ptr::null_mut(),
            )
        },
        ffi::SQLITE_OK
    );
    let (c, error) = c.close().err().unwrap();
    assert!(matches!(error, rusqlite::Error::SqliteFailure(_, _)));
    assert!(!unsafe { ffi::sqlite3_vfs_find(name.as_ptr()) }.is_null());
    assert_eq!(unsafe { ffi::sqlite3_finalize(stmt) }, ffi::SQLITE_OK);
    c.close().map_err(|(_, e)| e).unwrap();
    assert!(unsafe { ffi::sqlite3_vfs_find(name.as_ptr()) }.is_null());
    let raw = Connection::open_in_memory().unwrap();
    let owned: OwnedConnection = raw.into();
    owned
        .into_legacy()
        .unwrap()
        .execute_batch("SELECT 1")
        .unwrap();
    let f = fixture();
    assert!(f.connection.into_legacy().is_err());
}
#[test]
fn unsafe_policy_or_absent_owner_cannot_install_restriction() {
    let raw: OwnedConnection = Connection::open_in_memory().unwrap().into();
    assert!(raw.restrict(budget(1 << 20, 1 << 20, 1 << 20)).is_err());
    let f = fixture();
    let c = &f.connection;
    assert!(c.restrict(budget(1 << 20, 1 << 20, 1 << 20)).is_err());
    c.execute_batch("PRAGMA temp_store=FILE; BEGIN IMMEDIATE")
        .unwrap();
    assert!(c.restrict(budget(1 << 20, 1 << 20, 1 << 20)).is_err());
    assert!(LogicalBudget::new([0, 0, 0], 0, 0, 0).is_none());
    assert!(LogicalBudget::new([u64::MAX, 0, 0], 4096, 0, 0).is_none());
}

#[test]
fn another_real_sqlite_wal_filename_cannot_join_owner_context() {
    let a = fixture();
    let b = fixture();
    let r = a.connection.registration.as_ref().unwrap();
    // SAFETY: SQLite itself supplies the metadata-bearing filename, both owners remain alive.
    let filename = unsafe {
        ffi::sqlite3_filename_wal(ffi::sqlite3_db_filename(
            b.connection.handle(),
            c"main".as_ptr(),
        ))
    };
    assert!(!filename.is_null());
    let layout = std::alloc::Layout::from_size_align(r.vfs.szOsFile as usize, 8).unwrap();
    let output = unsafe { std::alloc::alloc_zeroed(layout) }.cast::<ffi::sqlite3_file>();
    assert!(!output.is_null());
    let mut flags = 0;
    let rc = unsafe {
        r.vfs.xOpen.unwrap()(
            (&*r.vfs as *const ffi::sqlite3_vfs).cast_mut(),
            filename,
            output,
            ffi::SQLITE_OPEN_WAL | ffi::SQLITE_OPEN_READWRITE | ffi::SQLITE_OPEN_CREATE,
            &mut flags,
        )
    };
    assert_eq!(rc, ffi::SQLITE_CANTOPEN);
    assert!(unsafe { (*output).pMethods }.is_null());
    unsafe { std::alloc::dealloc(output.cast(), layout) };
}
#[test]
fn sealed_opener_rejects_main_symlink_without_touching_its_target() {
    let temp = tempfile::tempdir().unwrap();
    let outside = temp.path().join("outside.db");
    Connection::open(&outside)
        .unwrap()
        .execute_batch("CREATE TABLE untouched(value)")
        .unwrap();
    let before = std::fs::read(&outside).unwrap();
    let alias = temp.path().join("alias.db");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, &alias).unwrap();
    #[cfg(windows)]
    {
        std::fs::create_dir(&alias).unwrap();
    }
    #[cfg(target_os = "linux")]
    let root = Dir::open_ambient_dir(temp.path(), cap_std::ambient_authority()).unwrap();
    #[cfg(target_os = "linux")]
    let path = std::path::PathBuf::from(format!("/proc/self/fd/{}/alias.db", root.as_raw_fd()));
    #[cfg(not(target_os = "linux"))]
    let path = alias;
    assert!(
        open_owned(
            path,
            OpenFlags::default() | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            None
        )
        .is_err()
    );
    assert_eq!(std::fs::read(&outside).unwrap(), before);
}
#[test]
fn real_unaligned_and_sparse_extent_growth_is_charged_before_forwarding() {
    let f = fixture();
    let c = &f.connection;
    c.execute_batch("BEGIN IMMEDIATE").unwrap();
    let wal = file(c, ffi::SQLITE_FCNTL_JOURNAL_POINTER);
    let attempt = c
        .restrict(LogicalBudget::new([0, 4096, 32768], 4096, 0, 0).unwrap())
        .unwrap();
    let before = size(wal);
    let bytes = [0u8; 4096];
    // The write spans two allocation units; a one-unit ceiling cannot admit it.
    assert_eq!(
        unsafe { (*(*wal).pMethods).xWrite.unwrap()(wal, bytes.as_ptr().cast(), 4096, 1) },
        ffi::SQLITE_FULL
    );
    assert_eq!(size(wal), before);
    drop(attempt);
    let f = fixture();
    let c = &f.connection;
    c.execute_batch("BEGIN IMMEDIATE").unwrap();
    let wal = file(c, ffi::SQLITE_FCNTL_JOURNAL_POINTER);
    let attempt = c.restrict(budget(0, 4096, 1 << 20)).unwrap();
    let before = size(wal);
    assert_eq!(
        unsafe { (*(*wal).pMethods).xWrite.unwrap()(wal, bytes.as_ptr().cast(), 1, 1 << 30) },
        ffi::SQLITE_FULL
    );
    assert_eq!(size(wal), before);
    assert_eq!(attempt.report().charged[1], 0);
}

#[test]
#[cfg(target_os = "linux")]
fn retained_owned_open_survives_original_root_descriptor_drop_through_close() {
    use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard, WritableProfileGuard};
    use crate::sample_sources::db::BoundDatabaseRoot;
    let _runtime = crate::test_runtime::TestRuntimeGuard::acquire();
    let config = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(config.path().to_path_buf());
    let _profile = PersistenceProfileGuard::named("owned-vfs-root");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let root = BoundDatabaseRoot::for_profile_guard(&guard).unwrap();
    let connection = root
        .open_owned_sqlite(std::path::Path::new("owned-root.db"), OpenFlags::default())
        .unwrap();
    let descriptor = connection
        .directory_descriptor
        .as_ref()
        .unwrap()
        .as_raw_fd();
    assert!(
        connection
            .path()
            .unwrap()
            .starts_with(&format!("/proc/self/fd/{descriptor}/"))
    );
    drop(root);
    // Make the original FD number reusable; every sidecar callback must keep using our own FD.
    let replacements: Vec<_> = (0..32)
        .map(|_| std::fs::File::open("/dev/null").unwrap())
        .collect();
    connection.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE held(value INTEGER); BEGIN IMMEDIATE; INSERT INTO held VALUES(42); COMMIT").unwrap();
    connection.close().map_err(|(_, e)| e).unwrap();
    drop(replacements);
    let reader = Connection::open(guard.profile_root().join("owned-root.db")).unwrap();
    assert_eq!(
        reader
            .query_row("SELECT value FROM held", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        42
    );
}

#[test]
fn sync_failure_retains_precharged_write_and_metadata_budget() {
    let f = fixture();
    let c = &f.connection;
    c.execute_batch("BEGIN IMMEDIATE").unwrap();
    let attempt = c.restrict(budget(1 << 20, 1 << 20, 1 << 20)).unwrap();
    c.registration
        .as_ref()
        .unwrap()
        .context
        .fail_sync
        .store(true, Ordering::SeqCst);
    assert!(
        c.execute_batch("INSERT INTO data VALUES(42); COMMIT")
            .is_err()
    );
    let report = attempt.report();
    assert_eq!(report.failure, Some(BudgetFailure::NativeIo));
    assert!(report.charged[1] >= 12288);
    c.execute_batch("ROLLBACK").ok();
    assert_eq!(attempt.report().charged, report.charged);
}

#[test]
fn implicit_busy_close_retains_registration_and_blocks_remaining_native_writes() {
    let f = fixture();
    let c = f.connection;
    let name = c.registration.as_ref().unwrap().name.clone();
    let main = file(&c, ffi::SQLITE_FCNTL_FILE_POINTER);
    let handle = unsafe { c.handle() };
    let mut stmt = ptr::null_mut();
    // SAFETY: deliberately retain an actual C statement to exercise the documented BUSY close.
    assert_eq!(
        unsafe {
            ffi::sqlite3_prepare_v2(handle, c"SELECT 1".as_ptr(), -1, &mut stmt, ptr::null_mut())
        },
        ffi::SQLITE_OK
    );
    drop(c);
    assert!(!unsafe { ffi::sqlite3_vfs_find(name.as_ptr()) }.is_null());
    let bytes = [0u8; 4096];
    let before = size(main);
    assert_ne!(
        unsafe { (*(*main).pMethods).xWrite.unwrap()(main, bytes.as_ptr().cast(), 4096, 0) },
        ffi::SQLITE_OK
    );
    assert_eq!(size(main), before);
    assert_eq!(unsafe { ffi::sqlite3_step(stmt) }, ffi::SQLITE_ROW);
    assert_eq!(unsafe { ffi::sqlite3_finalize(stmt) }, ffi::SQLITE_OK);
    assert_eq!(unsafe { ffi::sqlite3_close(handle) }, ffi::SQLITE_OK);
    // Its unconfirmed owner was intentionally retained; this test cannot claim a normal
    // owner-mediated reconciliation or free the forgotten Rust connection's bookkeeping.
}
