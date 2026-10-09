use super::*;

fn input() -> InputPermit {
    InputPermit::historical_test_namespace_input(1)
}

fn scope<'a>(connection: &'a Connection, input: &'a InputPermit) -> Scope<'a> {
    Scope::new(connection, input).unwrap_or_else(|_| panic!("original control scope"))
}

fn original_failure(connection: &Connection, sql: &str) -> rusqlite::Error {
    connection
        .execute_batch(sql)
        .expect_err("original control failure")
}

fn assert_same_native(error: &OperationJournalStoreError, original: &rusqlite::Error) {
    let OperationJournalStoreError::Sql(actual) = error else {
        panic!("retained original typed SQLite error");
    };
    match (actual, original) {
        (rusqlite::Error::SqliteFailure(a, am), rusqlite::Error::SqliteFailure(b, bm)) => {
            assert_eq!(a, b);
            assert_eq!(am, bm);
        }
        _ => panic!("original SQLite failure/code/message shape"),
    }
}

#[test]
fn funded_controls_match_same_original_connection_and_exact_refund() {
    let connection = Connection::open_in_memory().unwrap();
    let input = input();
    let baseline = input.historical_test_remaining();
    {
        let mut controls = scope(&connection, &input);
        assert_eq!(input.historical_test_remaining(), baseline - WORKING_BYTES);
        controls.begin().unwrap_or_else(|_| panic!("begin"));
        assert!(!connection.is_autocommit());
        controls.rollback().unwrap_or_else(|_| panic!("rollback"));
        assert!(connection.is_autocommit());
        assert_eq!(input.historical_test_remaining(), baseline - WORKING_BYTES);
    }
    assert_eq!(input.historical_test_remaining(), baseline);
    connection.execute_batch("BEGIN").unwrap();
    let original = original_failure(&connection, "BEGIN");
    let mut controls = scope(&connection, &input);
    let funded = controls.begin().err().unwrap();
    assert_same_native(funded.error.as_ref().unwrap(), &original);
    assert!(!connection.is_autocommit());
    assert!(input.historical_test_remaining() < baseline - WORKING_BYTES);
    drop(funded);
    drop(controls);
    assert_eq!(input.historical_test_remaining(), baseline);
    connection.execute_batch("ROLLBACK").unwrap();
    let original = original_failure(&connection, "ROLLBACK");
    let mut controls = scope(&connection, &input);
    let funded = controls.rollback().err().unwrap();
    assert_same_native(funded.error.as_ref().unwrap(), &original);
    drop(funded);
    drop(controls);
    assert_eq!(input.historical_test_remaining(), baseline);
}

#[test]
fn fixed_control_pressure_refuses_before_original_begin() {
    let connection = Connection::open_in_memory().unwrap();
    let input = input();
    let baseline = input.historical_test_remaining();
    let held = input
        .reserve_historical_scratch(baseline - (WORKING_BYTES - 1))
        .unwrap();
    let remaining = input.historical_test_remaining();
    let failure = Scope::new(&connection, &input).err().unwrap();
    assert!(matches!(
        failure.error,
        Some(OperationJournalStoreError::OrdinaryRebuildResourceBound)
    ));
    assert!(connection.is_autocommit());
    assert!(failure.native.is_none());
    assert!(failure.working.is_none());
    assert_eq!(input.historical_test_remaining(), remaining);
    drop(failure);
    drop(held);
    assert_eq!(input.historical_test_remaining(), baseline);
}

#[test]
fn ignored_secondary_rollback_never_refunds_primary_backing() {
    let connection = Connection::open_in_memory().unwrap();
    let input = input();
    let baseline = input.historical_test_remaining();
    let mut controls = scope(&connection, &input);
    let primary = controls.rollback().err().unwrap();
    let primary_remaining = input.historical_test_remaining();
    let secondary = run(&connection, &input, Control::Rollback).err().unwrap();
    assert!(input.historical_test_remaining() < primary_remaining);
    assert_same_native(
        primary.error.as_ref().unwrap(),
        &original_failure(&connection, "ROLLBACK"),
    );
    drop(secondary);
    assert_eq!(input.historical_test_remaining(), primary_remaining);
    controls.discard_rollback();
    assert_eq!(input.historical_test_remaining(), primary_remaining);
    assert_same_native(
        primary.error.as_ref().unwrap(),
        &original_failure(&connection, "ROLLBACK"),
    );
    drop(primary);
    drop(controls);
    assert_eq!(input.historical_test_remaining(), baseline);
}

#[test]
fn exact_native_copy_pressure_retains_no_unfunded_message() {
    let connection = Connection::open_in_memory().unwrap();
    let input = input();
    let baseline = input.historical_test_remaining();
    let mut controls = scope(&connection, &input);
    let measured = controls.rollback().err().unwrap();
    let native_bytes = measured
        .native
        .as_ref()
        .expect("original native message loan")
        .historical_test_funded_bytes();
    drop(measured);
    drop(controls);
    assert_eq!(input.historical_test_remaining(), baseline);
    let mut controls = scope(&connection, &input);
    let held = input
        .reserve_historical_scratch(baseline - WORKING_BYTES - (native_bytes - 1))
        .unwrap();
    let remaining = input.historical_test_remaining();
    let refused = controls.rollback().err().unwrap();
    assert!(matches!(
        refused.error,
        Some(OperationJournalStoreError::HistoricalClaimResourceBound)
    ));
    assert!(refused.native.is_none());
    assert_eq!(input.historical_test_remaining(), remaining);
    assert!(connection.is_autocommit());
    drop(refused);
    assert_eq!(input.historical_test_remaining(), remaining + WORKING_BYTES);
    drop(held);
    drop(controls);
    assert_eq!(input.historical_test_remaining(), baseline);
}

#[test]
fn ignored_rollback_still_restores_original_autocommit_under_pressure() {
    let connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch("CREATE TABLE original(value INTEGER); INSERT INTO original VALUES(7)")
        .unwrap();
    let input = input();
    let baseline = input.historical_test_remaining();
    let mut controls = scope(&connection, &input);
    controls
        .begin()
        .unwrap_or_else(|_| panic!("original BEGIN"));
    let held = input
        .reserve_historical_scratch(input.historical_test_remaining() - 1)
        .unwrap();
    controls.discard_rollback();
    assert!(connection.is_autocommit());
    assert_eq!(input.historical_test_remaining(), 1);
    // SAFETY: same original thread-confined connection, no statement/handle escapes.
    assert!(
        unsafe { rusqlite::ffi::sqlite3_next_stmt(connection.handle(), std::ptr::null_mut()) }
            .is_null()
    );
    let value: i64 = connection
        .query_row("SELECT value FROM original", [], |row| row.get(0))
        .unwrap();
    assert_eq!(value, 7);
    drop(held);
    assert_eq!(input.historical_test_remaining(), baseline - WORKING_BYTES);
    drop(controls);
    assert_eq!(input.historical_test_remaining(), baseline);
}

#[test]
fn missing_original_reply_refuses_without_transaction_or_owned_error() {
    let connection = Connection::open_in_memory().unwrap();
    let input = InputPermit::historical_test_input(1);
    let baseline = input.historical_test_remaining();
    let failure = Scope::new(&connection, &input).err().unwrap();
    assert!(matches!(
        failure.error,
        Some(OperationJournalStoreError::OrdinaryRebuildResourceBound)
    ));
    assert!(failure.native.is_none());
    assert!(failure.working.is_none());
    assert!(connection.is_autocommit());
    drop(failure);
    assert_eq!(input.historical_test_remaining(), baseline);
}

#[test]
fn original_control_statement_unwind_drops_payload_before_exact_loan_refund() {
    let connection = Connection::open_in_memory().unwrap();
    let input = input();
    let baseline = input.historical_test_remaining();
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut controls = scope(&connection, &input);
        let failure = controls.rollback().err().unwrap();
        assert!(failure.native.is_some());
        assert!(input.historical_test_remaining() < baseline - WORKING_BYTES);
        panic!("unwind while original primary native payload and loans coexist");
    }));
    assert!(caught.is_err());
    assert_eq!(input.historical_test_remaining(), baseline);
    assert!(connection.is_autocommit());
    // SAFETY: same original thread-confined connection; no handle escapes.
    assert!(
        unsafe { rusqlite::ffi::sqlite3_next_stmt(connection.handle(), std::ptr::null_mut()) }
            .is_null()
    );
}

#[test]
fn original_reader_control_fault_allows_reserve_transactions_and_keeps_failed_read_backing() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    let _runtime = crate::test_runtime::TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _root =
        crate::app_dirs::AppRootGuard::set(std::fs::canonicalize(temp.path()).unwrap()).unwrap();
    let profile = crate::app_dirs::WritableProfileGuard::acquire_current().unwrap();
    let store = OperationJournalStore::open(&profile).unwrap();
    store.deny_original_reader_begin_for_test();
    store.connection.execute_batch("BEGIN; ROLLBACK;").unwrap();
    let funding = JournalNamespaceFundingProbe::new();
    let mut reader = Reader::new(&store.connection, Some(&funding.input)).unwrap();
    let error = reader.begin().unwrap_err();
    assert!(format!("{error}").contains("not authorized"));
    assert!(store.connection.is_autocommit());
    drop(reader);
    // The scoped marker resets after failure; ordinary original transactions
    // remain usable and no unrelated bootstrap denial is substituted.
    store.connection.execute_batch("BEGIN; ROLLBACK;").unwrap();
    let pool = funding.pool_bytes_observer_for_test();
    let error = funding.retain_command_error_for_test(error);
    drop(funding);
    assert!(pool().0 < pool().1);
    drop(error);
    assert_eq!(pool().0, pool().1);
}
