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
    pub(crate) fn snapshot(&self) -> CapacityAdmissionTelemetrySnapshot {
        self.0.snapshot()
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        CAPTURE.with(|slot| *slot.borrow_mut() = None);
    }
}
pub(super) fn record(decision: CapacityAdmissionDecision, elapsed: Duration) {
    CAPTURE.with(|slot| {
        if let Some(counters) = slot.borrow().as_ref() {
            counters.record(decision, elapsed);
        }
    });
}
#[test]
fn fixed_numeric_collector_saturates_and_preserves_bucket_boundaries() {
    assert_eq!(std::mem::size_of::<Counters>(), 136);
    let counters = Counters::new();
    for millis in [0, 1, 2, 10, 11, 100, 101, 1000, 1001] {
        counters.record(
            CapacityAdmissionDecision::Unavailable,
            Duration::from_millis(millis),
        );
    }
    assert_eq!(counters.snapshot().elapsed_buckets, [2, 2, 2, 2, 1]);
    counters.counts[CapacityAdmissionDecision::Unavailable as usize]
        .store(u64::MAX, Ordering::Relaxed);
    counters.elapsed[0].store(u64::MAX, Ordering::Relaxed);
    counters.record(CapacityAdmissionDecision::Unavailable, Duration::ZERO);
    assert_eq!(
        counters
            .snapshot()
            .count(CapacityAdmissionDecision::Unavailable),
        u64::MAX
    );
    assert_eq!(counters.snapshot().elapsed_buckets[0], u64::MAX);
}
