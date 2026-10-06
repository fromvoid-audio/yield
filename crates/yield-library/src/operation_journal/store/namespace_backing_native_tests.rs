// Native fault verification on the SAME original Store/Connection. NULL_IO is a
// documented SQLite file control, not a fake VFS or a replacement database.
#[cfg(windows)]
#[test]
fn namespace_backing_native_original_connection_refusal() {
    const CHILD_ROOT: &str = "YIELD_NAMESPACE_NATIVE_FAULT_ROOT";
    const MARKER: &str = "original native namespace fault verified before process teardown";
    let Some(root) = std::env::var_os(CHILD_ROOT) else {
        let root = tempfile::tempdir().unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("operation_journal::store::tests::namespace_backing_native_original_connection_refusal")
            .arg("--nocapture")
            .env(CHILD_ROOT, root.path())
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "native fault child failed: {result:?}"
        );
        assert!(String::from_utf8_lossy(&result.stdout).contains(MARKER));
        // Only this parent owns the temporary tree. Child exit releases all OS
        // handles before TempDir removes it. No production registry is involved.
        return;
    };
    let _runtime = TestRuntimeGuard::acquire();
    let _config = ConfigBaseGuard::set(fs::canonicalize(root).unwrap());
    let _profile = PersistenceProfileGuard::named("native-namespace-fault".to_owned());
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let store = OperationJournalStore::open(&profile).unwrap();
    let before = fs::read(&store.database_path).unwrap();
    let ids: i64 = store
        .connection
        .query_row("SELECT COUNT(*) FROM operation_journal", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(ids, 0);
    assert!(store.validate_namespace().is_ok());
    // From immediately before the destructive control, no assertion failure or
    // unwind may invoke winClose on its invalid handle. Only this child leaks
    // the original Store until process teardown; all diagnostic credits still
    // have their normal Rust destruction paths.
    let store = std::mem::ManuallyDrop::new(store);
    let result = unsafe {
        rusqlite::ffi::sqlite3_file_control(
            store.connection.handle(),
            c"main".as_ptr(),
            rusqlite::ffi::SQLITE_FCNTL_NULL_IO,
            std::ptr::null_mut(),
        )
    };
    assert_eq!(result, rusqlite::ffi::SQLITE_OK);
    // No SQL or VFS I/O after NULL_IO. Querying its actual original native handle
    // exercises the real production WIN32_GET_HANDLE refusal path.
    let original = store.validate_namespace().unwrap_err();
    let OperationJournalStoreError::IdentityCheckUnavailable { ref reason } = original else {
        panic!("original invalid native handle error changed: {original:?}")
    };
    assert_eq!(reason, "SQLite returned an invalid native file handle");
    let expected = original.to_string();
    let reason_bytes = reason.len();
    drop(original);
    let funding = super::super::owner::JournalNamespaceFundingProbe::new();
    let observe_pool = funding.pool_bytes_observer_for_test();
    let (available, baseline) = observe_pool();
    let input = &funding.input;
    let working = store.retained_namespace_working_bytes_for_test();
    let native = reason_bytes + 3 * (reason_bytes * 2).max(8);
    let funded = store
        .validate_namespace_with_input(Some(input), false)
        .unwrap_err();
    assert!(matches!(
        funded,
        OperationJournalStoreError::IdentityCheckUnavailable { .. }
    ));
    assert_eq!(funded.to_string(), expected);
    assert_eq!(
        input.historical_test_remaining(),
        available - working - native
    );
    drop(funded);
    assert_eq!(
        input.historical_test_remaining(),
        available - working - native,
        "the original diagnostic consumer still owns selected native backing"
    );
    assert_eq!(fs::read(&store.database_path).unwrap(), before);
    drop(funding); // Original selected error, reply allocation and credits.
    assert_eq!(
        observe_pool().0,
        baseline,
        "exact original post-destruction refund"
    );
    println!("{MARKER}");
    std::io::Write::flush(&mut std::io::stdout()).unwrap();
    // winClose asserts that the NULL_IO faulted handle is non-null. This child
    // intentionally exits before invalid-VFS destruction; the parent checks the
    // real exit and cleans its tree after OS handle cleanup. This fault harness
    // does not claim normal Store/worker shutdown or physical qualification.
    std::process::exit(0);
}
