use super::*;
use std::{cell::RefCell, sync::Arc};

thread_local! { static CAPTURE: RefCell<Option<Arc<Counters>>> = const { RefCell::new(None) }; }
pub(crate) struct Capture(Arc<Counters>);
impl Capture {
    pub(crate) fn new() -> Self {
        let counters = Arc::new(Counters::new());
        CAPTURE.with(|slot| {
            assert!(slot.borrow().is_none());
            *slot.borrow_mut() = Some(counters.clone());
        });
        Self(counters)
    }
    pub(crate) fn snapshot(&self) -> JournalWriterTelemetrySnapshot {
        self.0.snapshot()
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        CAPTURE.with(|slot| *slot.borrow_mut() = None);
    }
}
pub(super) fn record(call: JournalWriterCall, result: JournalWriterReturn, elapsed: Duration) {
    CAPTURE.with(|slot| {
        if let Some(counters) = slot.borrow().as_ref() {
            counters.record(call, result, elapsed);
        }
    });
}

#[test]
fn fixed_numeric_backing_saturation_and_elapsed_boundaries() {
    assert_eq!(std::mem::size_of::<Counters>(), 168);
    assert_eq!(std::mem::size_of::<JournalWriterTelemetrySnapshot>(), 168);
    let counters = Counters::new();
    for millis in [0, 1, 2, 10, 11, 100, 101, 1000, 1001] {
        counters.record(
            JournalWriterCall::IntentInsert,
            JournalWriterReturn::Acquired,
            Duration::from_millis(millis),
        );
    }
    assert_eq!(counters.snapshot().elapsed_buckets, [2, 2, 2, 2, 1]);
    for counter in counters.counts.iter().flatten().chain(&counters.elapsed) {
        counter.store(u64::MAX, Ordering::Relaxed);
    }
    for call in [
        JournalWriterCall::IntentInsert,
        JournalWriterCall::CancellationRequest,
        JournalWriterCall::RetryPending,
        JournalWriterCall::OrdinaryClaimRelease,
    ] {
        for outcome in [
            JournalWriterReturn::Acquired,
            JournalWriterReturn::Busy,
            JournalWriterReturn::Locked,
            JournalWriterReturn::OtherError,
        ] {
            counters.record(call, outcome, Duration::ZERO);
            assert_eq!(counters.snapshot().count(call, outcome), u64::MAX);
        }
    }
    assert_eq!(counters.snapshot().elapsed_buckets, [u64::MAX; 5]);
}

#[test]
fn extended_sqlite_contention_and_original_diagnostic_are_preserved() {
    for (code, expected) in [
        (rusqlite::ffi::SQLITE_BUSY, JournalWriterReturn::Busy),
        (
            rusqlite::ffi::SQLITE_BUSY_RECOVERY,
            JournalWriterReturn::Busy,
        ),
        (rusqlite::ffi::SQLITE_LOCKED, JournalWriterReturn::Locked),
        (
            rusqlite::ffi::SQLITE_LOCKED_SHAREDCACHE,
            JournalWriterReturn::Locked,
        ),
        (rusqlite::ffi::SQLITE_AUTH, JournalWriterReturn::OtherError),
    ] {
        let message = String::from("original native diagnostic");
        let pointer = message.as_ptr();
        let capture = Capture::new();
        let result = observe(JournalWriterCall::IntentInsert, || {
            Err(OperationJournalStoreError::Sql(
                rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), Some(message)),
            ))
        });
        assert_eq!(category(&result), expected);
        assert!(
            matches!(result, Err(OperationJournalStoreError::Sql(rusqlite::Error::SqliteFailure(error, Some(message)))) if error.extended_code == code && message.as_ptr() == pointer)
        );
        assert_eq!(
            capture
                .snapshot()
                .count(JournalWriterCall::IntentInsert, expected),
            1
        );
    }
}

#[test]
fn real_sqlite_acquisition_and_nested_refusal_keep_original_transaction() {
    let connection = rusqlite::Connection::open_in_memory().unwrap();
    connection
        .execute_batch("CREATE TABLE unchanged(value INTEGER); INSERT INTO unchanged VALUES(7)")
        .unwrap();
    let capture = Capture::new();
    let begin = || {
        connection
            .execute_batch("BEGIN EXCLUSIVE")
            .map_err(Into::into)
    };
    observe(JournalWriterCall::IntentInsert, begin).unwrap();
    assert!(!connection.is_autocommit());
    observe(JournalWriterCall::IntentInsert, begin).expect_err("actual nested transaction refusal");
    assert!(!connection.is_autocommit());
    connection.execute_batch("ROLLBACK").unwrap();
    assert_eq!(
        connection
            .query_row("SELECT value FROM unchanged", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        7
    );
    let snapshot = capture.snapshot();
    assert_eq!(
        snapshot.count(
            JournalWriterCall::IntentInsert,
            JournalWriterReturn::Acquired
        ),
        1
    );
    assert_eq!(
        snapshot.count(
            JournalWriterCall::IntentInsert,
            JournalWriterReturn::OtherError
        ),
        1
    );
    assert_eq!(snapshot.elapsed_buckets.iter().sum::<u64>(), 2);
}
