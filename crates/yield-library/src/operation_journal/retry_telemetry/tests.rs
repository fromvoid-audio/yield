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
    pub(crate) fn snapshot(&self) -> JournalRetryTelemetrySnapshot {
        self.0.snapshot()
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        CAPTURE.with(|slot| *slot.borrow_mut() = None);
    }
}
pub(super) fn record(call: JournalRetryCall, result: JournalRetryReturn, elapsed: Duration) {
    CAPTURE.with(|slot| {
        if let Some(counters) = slot.borrow().as_ref() {
            counters.record(call, result, elapsed);
        }
    });
}

#[test]
fn fixed_numeric_backing_saturation_and_elapsed_boundaries() {
    assert_eq!(std::mem::size_of::<Counters>(), 104);
    assert_eq!(std::mem::size_of::<JournalRetryTelemetrySnapshot>(), 104);
    let counters = Counters::new();
    for millis in [0, 1, 2, 10, 11, 100, 101, 1000, 1001] {
        counters.record(
            JournalRetryCall::RetryPending,
            JournalRetryReturn::ConfirmedReturn,
            Duration::from_millis(millis),
        );
    }
    assert_eq!(counters.snapshot().elapsed_buckets, [2, 2, 2, 2, 1]);
    counters.counts[0][0].store(u64::MAX, Ordering::Relaxed);
    counters.elapsed[0].store(u64::MAX, Ordering::Relaxed);
    counters.record(
        JournalRetryCall::RetryPending,
        JournalRetryReturn::ConfirmedReturn,
        Duration::ZERO,
    );
    assert_eq!(counters.snapshot().counts[0][0], u64::MAX);
    assert_eq!(counters.snapshot().elapsed_buckets[0], u64::MAX);
}

#[test]
fn exact_extended_contention_codes_and_original_owned_results() {
    for code in [
        rusqlite::ffi::SQLITE_BUSY,
        rusqlite::ffi::SQLITE_BUSY_RECOVERY,
        rusqlite::ffi::SQLITE_LOCKED,
        rusqlite::ffi::SQLITE_LOCKED_SHAREDCACHE,
    ] {
        let result: Result<(), _> = Err(OperationJournalStoreError::Sql(
            rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None),
        ));
        assert_eq!(category(&result), JournalRetryReturn::ContentionError);
    }
    struct Unique(Arc<AtomicU64>);
    impl Drop for Unique {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    let dropped = Arc::new(AtomicU64::new(0));
    let returned = observe(JournalRetryCall::RetryPending, || {
        Ok(Unique(dropped.clone()))
    })
    .expect("original unique result");
    assert_eq!(dropped.load(Ordering::Relaxed), 0);
    drop(returned);
    assert_eq!(dropped.load(Ordering::Relaxed), 1);

    let reason = String::from("original retained diagnostic");
    let pointer = reason.as_ptr();
    let returned: Result<(), _> = observe(JournalRetryCall::RetryPending, || {
        Err(OperationJournalStoreError::RetainedRecord {
            operation_id: None,
            reason,
        })
    });
    assert!(
        matches!(returned, Err(OperationJournalStoreError::RetainedRecord { reason, .. }) if reason.as_ptr() == pointer)
    );
}
