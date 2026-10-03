use super::*;
use crate::app_dirs::AppRootGuard;
fn fixture() -> (
    tempfile::TempDir,
    AppRootGuard,
    WritableProfileGuard,
    OperationJournalStore,
) {
    let directory = tempfile::tempdir().unwrap();
    let root = AppRootGuard::set(directory.path().to_path_buf()).unwrap();
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let store = OperationJournalStore::open(&guard).unwrap();
    (directory, root, guard, store)
}
#[test]
fn observation_preserves_actual_connection_policy_and_smaller_page_cap() {
    let _runtime = crate::test_runtime::TestRuntimeGuard::acquire();
    let (_dir, _root, guard, store) = fixture();
    let size: u64 = store
        .connection
        .pragma_query_value(None, "page_size", |r| r.get(0))
        .unwrap();
    let count: u64 = store
        .connection
        .pragma_query_value(None, "page_count", |r| r.get(0))
        .unwrap();
    let cap = count + 5;
    let actual: u64 = store
        .connection
        .pragma_query_value(None, "max_page_count", |r| r.get(0))
        .unwrap();
    assert!(actual > cap);
    store
        .connection
        .execute_batch(&format!(
            "PRAGMA max_page_count={cap}; PRAGMA cache_spill=ON; PRAGMA temp_store=DEFAULT;"
        ))
        .unwrap();
    let before = std::fs::read(&store.database_path).unwrap();
    let result = store.capture_legacy_journal_observation(&guard).unwrap();
    assert_eq!(result.selected_max_pages(), cap);
    assert_eq!(result.proposed_database_bytes(), size * cap);
    assert!(result.current_cache_spill() > 0);
    assert_eq!(result.current_temp_store(), 0);
    assert_eq!(
        store
            .connection
            .pragma_query_value(None, "max_page_count", |r| r.get::<_, u64>(0))
            .unwrap(),
        cap
    );
    assert!(
        store
            .connection
            .pragma_query_value(None, "cache_spill", |r| r.get::<_, u64>(0))
            .unwrap()
            > 0
    );
    assert_eq!(std::fs::read(&store.database_path).unwrap(), before);
}
#[test]
fn deadline_and_unknown_mode_schema_or_attachment_refuse_without_observer_mutation() {
    let _runtime = crate::test_runtime::TestRuntimeGuard::acquire();
    let (_dir, _root, guard, store) = fixture();
    let before = std::fs::read(&store.database_path).unwrap();
    assert!(
        store
            .capture_legacy_journal_until(&guard, Instant::now() - Duration::from_millis(1))
            .is_err()
    );
    assert_eq!(std::fs::read(&store.database_path).unwrap(), before);
    store
        .connection
        .execute_batch("PRAGMA journal_mode=MEMORY")
        .unwrap();
    assert!(store.capture_legacy_journal_observation(&guard).is_err());
    store
        .connection
        .execute_batch("PRAGMA journal_mode=DELETE; CREATE TEMP TABLE forbidden(x);")
        .unwrap();
    assert!(store.capture_legacy_journal_observation(&guard).is_err());
    store
        .connection
        .execute_batch("DROP TABLE temp.forbidden; ATTACH ':memory:' AS extra;")
        .unwrap();
    assert!(store.capture_legacy_journal_observation(&guard).is_err());
    store
        .connection
        .execute_batch("DETACH extra; CREATE TABLE unknown(x);")
        .unwrap();
    let before = std::fs::read(&store.database_path).unwrap();
    assert!(store.capture_legacy_journal_observation(&guard).is_err());
    assert_eq!(std::fs::read(&store.database_path).unwrap(), before);
    assert!(store.connection.is_autocommit());
}
#[test]
fn selected_policy_and_overflow_never_become_zero_requirements() {
    assert_eq!(select(512, 1, 1, 512), Some(1));
    for args in [
        (0, 1, 1, 1),
        (513, 1, 1, 513),
        (512, 0, 1, 0),
        (512, 2, 1, 1024),
        (512, 1, 1, 513),
    ] {
        assert_eq!(select(args.0, args.1, args.2, args.3), None);
    }
    assert_eq!(peak(u64::MAX, 2), None);
    assert_eq!(peak(512, u64::MAX), None);
    assert_eq!(peak(0, 1), None);
    let raw = Connection::open_in_memory().unwrap();
    assert_eq!(native_facts(&raw), None);
}

#[test]
fn retained_profile_control_overflow_and_active_transaction_are_not_zero_observations() {
    let _runtime = crate::test_runtime::TestRuntimeGuard::acquire();
    let (directory, _root, guard, store) = fixture();
    assert!(store.capture_legacy_journal_observation(&guard).is_ok());
    store.connection.execute_batch("BEGIN").unwrap();
    assert!(store.capture_legacy_journal_observation(&guard).is_err());
    assert!(
        !store.connection.is_autocommit(),
        "observer must not rollback a foreign transaction"
    );
    store.connection.execute_batch("ROLLBACK").unwrap();
    std::fs::write(
        directory.path().join("profile-owner.lock"),
        vec![b'x'; 4097],
    )
    .unwrap();
    assert!(store.capture_legacy_journal_observation(&guard).is_err());
    assert_eq!(
        std::fs::metadata(directory.path().join("profile-owner.lock"))
            .unwrap()
            .len(),
        4097
    );
}
#[test]
fn journal_tainted_runtime_child() {
    if std::env::var_os("YIELD_LEGACY_JOURNAL_TAINT_CHILD").is_none() {
        return;
    }
    let _runtime = crate::test_runtime::TestRuntimeGuard::acquire();
    let (_dir, _root, guard, store) = fixture();
    assert!(store.capture_legacy_journal_observation(&guard).is_ok());
    let bytes = std::fs::read(&store.database_path).unwrap();
    assert!(
        crate::sqlite_ext::load_extension_for_test(
            &store.connection,
            Path::new("/nonexistent/yield-legacy-journal-extension.so")
        )
        .is_err()
    );
    assert!(store.capture_legacy_journal_observation(&guard).is_err());
    assert_eq!(std::fs::read(&store.database_path).unwrap(), bytes);
}
#[test]
fn journal_tainted_runtime_refuses_capture_in_actual_isolated_process() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact","operation_journal::store::legacy_allocation_observation::tests::journal_tainted_runtime_child","--test-threads=1"])
        .env("YIELD_LEGACY_JOURNAL_TAINT_CHILD","1").output().unwrap();
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
}

#[cfg(unix)]
#[test]
fn actual_descriptor_observation_open_refuses_fifo_and_symlink_without_waiting() {
    let _runtime = crate::test_runtime::TestRuntimeGuard::acquire();
    let (_dir, _root, _guard, store) = fixture();
    use std::os::unix::fs::FileTypeExt;
    let name = Path::new("observation-fifo");
    let path = store.database_root.path().join(name);
    let encoded =
        std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str())).unwrap();
    // SAFETY: NUL-terminated test pathname; mkfifo retains no borrowed pointer.
    assert_eq!(unsafe { libc::mkfifo(encoded.as_ptr(), 0o600) }, 0);
    assert!(
        store
            .database_root
            .open_observation_file_nofollow(name)
            .is_err()
    );
    assert!(
        std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_fifo()
    );
    std::os::unix::fs::symlink(
        &store.database_path,
        store.database_root.path().join("observation-link"),
    )
    .unwrap();
    assert!(
        store
            .database_root
            .open_observation_file_nofollow(Path::new("observation-link"))
            .is_err()
    );
    assert!(
        store
            .database_root
            .open_observation_file_nofollow(Path::new("../escape"))
            .is_err()
    );
}
