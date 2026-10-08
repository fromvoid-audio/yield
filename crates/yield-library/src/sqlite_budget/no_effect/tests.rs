use super::*;

fn open(path: &std::path::Path) -> rusqlite::Result<OwnedConnection> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let directory = std::sync::Arc::new(std::fs::File::open(path.parent().unwrap()).unwrap());
        let alias = std::path::PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()))
            .join(path.file_name().unwrap());
        open_existing_no_effect(alias, Some(directory))
    }
    #[cfg(not(target_os = "linux"))]
    {
        open_existing_no_effect(path, None)
    }
}

fn database() -> (tempfile::TempDir, std::path::PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("original.db");
    Connection::open(&path).unwrap().execute_batch(
        "PRAGMA journal_mode=DELETE; CREATE TABLE retained(value INTEGER); INSERT INTO retained VALUES(7);"
    ).unwrap();
    (temp, path)
}

#[test]
fn no_effect_existing_rw_sql_and_native_callbacks_preserve_original_bytes() {
    let (_temp, path) = database();
    let before = std::fs::read(&path).unwrap();
    let owner = open(&path).unwrap();
    assert!(owner.has_no_effect_fence());
    assert!(!owner.is_readonly(rusqlite::MAIN_DB).unwrap());
    owner
        .execute_batch("PRAGMA query_only=OFF; PRAGMA mmap_size=1048576;")
        .unwrap();
    assert_eq!(
        owner
            .query_row("SELECT value FROM retained", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        7
    );
    assert!(owner.execute("INSERT INTO retained VALUES(8)", []).is_err());
    assert!(owner.execute_batch("PRAGMA journal_mode=WAL").is_err());
    let main = owner
        .registration
        .as_ref()
        .unwrap()
        .context
        .main
        .load(Ordering::SeqCst);
    // SAFETY: actual private wrapper and its callbacks remain alive through the owned connection.
    unsafe {
        let methods = &*(*main).pMethods;
        let value = [0u8; 8];
        assert_eq!(
            methods.xWrite.unwrap()(main, value.as_ptr().cast(), 8, 0),
            ffi::SQLITE_READONLY
        );
        assert_eq!(methods.xTruncate.unwrap()(main, 0), ffi::SQLITE_READONLY);
        assert_eq!(
            methods.xSync.unwrap()(main, ffi::SQLITE_SYNC_FULL),
            ffi::SQLITE_READONLY
        );
        let mut mapped = ptr::null_mut();
        assert_eq!(
            methods.xShmMap.unwrap()(main, 0, 32768, 1, &mut mapped),
            ffi::SQLITE_READONLY
        );
        assert!(mapped.is_null());
        assert_eq!(methods.xShmUnmap.unwrap()(main, 1), ffi::SQLITE_READONLY);
        assert_eq!(
            methods.xFileControl.unwrap()(main, ffi::SQLITE_FCNTL_SIZE_HINT, ptr::null_mut()),
            ffi::SQLITE_NOTFOUND
        );
        assert_eq!(
            methods.xFileControl.unwrap()(main, 99999, ptr::null_mut()),
            ffi::SQLITE_NOTFOUND
        );
        let registration = owner.registration.as_ref().unwrap();
        assert_eq!(
            registration.vfs.xDelete.unwrap()(
                (&*registration.vfs as *const ffi::sqlite3_vfs).cast_mut(),
                c"original.db".as_ptr(),
                1
            ),
            ffi::SQLITE_READONLY
        );
    }
    assert_eq!(
        owner
            .registration
            .as_ref()
            .unwrap()
            .context
            .native_effect_calls
            .load(Ordering::SeqCst),
        0
    );
    drop(owner);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    for suffix in ["-journal", "-wal", "-shm"] {
        assert!(!path.with_file_name(format!("original.db{suffix}")).exists());
    }
}

#[test]
fn no_effect_missing_main_never_creates_file() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("missing.db");
    assert!(open(&path).is_err());
    assert!(!path.exists());
}

#[test]
fn no_effect_genuine_hot_rollback_cannot_replay_before_or_after_open() {
    const MARKER: &str = "YIELD_NO_EFFECT_HOT_JOURNAL_CHILD";
    if let Some(path) = std::env::var_os(MARKER) {
        let connection = Connection::open(path).unwrap();
        connection.execute_batch("PRAGMA cache_size=5; PRAGMA cache_spill=ON; BEGIN IMMEDIATE; UPDATE retained SET payload=zeroblob(4096);").unwrap();
        // Deliberately skip SQLite close/rollback. The parent checks real hot-journal bytes.
        std::process::exit(0);
    }
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("hot.db");
    Connection::open(&path).unwrap().execute_batch("PRAGMA journal_mode=DELETE; CREATE TABLE retained(payload BLOB); WITH RECURSIVE rows(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM rows WHERE n<128) INSERT INTO retained SELECT randomblob(4096) FROM rows;").unwrap();
    let committed = std::fs::read(&path).unwrap();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "sqlite_budget::no_effect::tests::no_effect_genuine_hot_rollback_cannot_replay_before_or_after_open", "--test-threads=1"])
        .env(MARKER, &path).output().unwrap();
    assert!(
        result.status.success(),
        "{} {}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    let journal = temp.path().join("hot.db-journal");
    let evidence = std::fs::read(&journal).unwrap();
    assert_eq!(
        &evidence[..8],
        &[0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7]
    );
    let dirty = std::fs::read(&path).unwrap();
    assert_ne!(dirty, committed, "real spill must precede crash");
    if let Ok(owner) = open(&path) {
        let observed = owner.query_row("SELECT count(*) FROM retained", [], |r| r.get::<_, i64>(0));
        assert!(
            observed.is_err(),
            "automatic recovery succeeded: {observed:?}; committed MAIN restored={}",
            std::fs::read(&path).unwrap() == committed
        );
        assert_eq!(
            owner
                .registration
                .as_ref()
                .unwrap()
                .context
                .native_effect_calls
                .load(Ordering::SeqCst),
            0
        );
        drop(owner);
    }
    assert_eq!(std::fs::read(&path).unwrap(), dirty);
    assert_eq!(std::fs::read(&journal).unwrap(), evidence);
    // Independent ordinary SQLite proves the retained journal really authorizes its recovery.
    let recovered = Connection::open(&path).unwrap();
    assert_eq!(
        recovered
            .query_row("SELECT count(*) FROM retained", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        128
    );
    drop(recovered);
    assert_eq!(std::fs::read(&path).unwrap(), committed);
}

#[test]
fn original_legacy_read_fence_rejects_changed_policy_open_transaction_and_poison() {
    let (_temp, path) = database();
    let before = std::fs::read(&path).unwrap();
    let owner = open(&path).unwrap();
    assert!(!owner.validate_original_legacy_read_fence());
    owner.execute_batch("PRAGMA query_only=ON; PRAGMA temp_store=MEMORY; PRAGMA cache_spill=OFF; PRAGMA mmap_size=0").unwrap();
    assert!(owner.validate_original_legacy_read_fence());
    assert_eq!(scalar(&owner, "mmap_size").unwrap(), 0);
    owner.execute_batch("PRAGMA mmap_size=1048576").unwrap();
    assert_eq!(scalar(&owner, "mmap_size").unwrap(), 0);
    assert!(owner.validate_original_legacy_read_fence());
    owner.execute_batch("BEGIN").unwrap();
    assert!(!owner.validate_original_legacy_read_fence());
    owner.execute_batch("ROLLBACK").unwrap();
    assert!(owner.validate_original_legacy_read_fence());
    for changed in [
        "PRAGMA query_only=OFF",
        "PRAGMA cache_spill=ON",
        "PRAGMA temp_store=FILE",
    ] {
        owner.execute_batch(changed).unwrap();
        assert!(!owner.validate_original_legacy_read_fence());
        owner
            .execute_batch("PRAGMA query_only=ON; PRAGMA temp_store=MEMORY; PRAGMA cache_spill=OFF")
            .unwrap();
        assert!(owner.validate_original_legacy_read_fence());
    }
    owner.registration.as_ref().unwrap().context.quarantine();
    assert!(!owner.validate_original_legacy_read_fence());
    assert!(owner.has_no_effect_fence());
    assert!(owner.execute("INSERT INTO retained VALUES(8)", []).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
}
