use super::*;
use std::{cell::RefCell, sync::Arc};
thread_local! { static CAPTURE: RefCell<Option<Arc<Counters>>> = const { RefCell::new(None) }; }
pub(crate) struct Capture(Arc<Counters>);
impl Capture {
    pub(crate) fn new() -> Self {
        let value = Arc::new(Counters::new());
        CAPTURE.with(|slot| {
            assert!(slot.borrow().is_none());
            *slot.borrow_mut() = Some(value.clone());
        });
        Self(value)
    }
    pub(crate) fn snapshot(&self) -> CapacityRetentionTelemetrySnapshot {
        self.0.snapshot()
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        CAPTURE.with(|slot| *slot.borrow_mut() = None);
    }
}
pub(super) fn record(call: CapacityLedgerCall, result: CapacityLedgerReturn, elapsed: Duration) {
    CAPTURE.with(|slot| {
        if let Some(value) = slot.borrow().as_ref() {
            value.record(call, result, elapsed);
        }
    });
}
#[test]
fn fixed_numeric_backing_saturation_and_buckets() {
    assert_eq!(std::mem::size_of::<Counters>(), 200);
    let value = Counters::new();
    let call = CapacityLedgerCall::ProvisionalRelease;
    let result = CapacityLedgerReturn::Confirmed;
    for millis in [0, 1, 2, 10, 11, 100, 101, 1000, 1001] {
        value.record(call, result, Duration::from_millis(millis));
    }
    assert_eq!(value.snapshot().elapsed_buckets, [2, 2, 2, 2, 1]);
    value.counts[call as usize][result as usize].store(u64::MAX, Ordering::Relaxed);
    value.elapsed[0].store(u64::MAX, Ordering::Relaxed);
    value.record(call, result, Duration::ZERO);
    assert_eq!(value.snapshot().count(call, result), u64::MAX);
    assert_eq!(value.snapshot().elapsed_buckets[0], u64::MAX);
}
