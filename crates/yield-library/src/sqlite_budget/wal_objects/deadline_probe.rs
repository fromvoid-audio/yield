//! One-shot original-worker scheduling gate. No observation or authority is fabricated.
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

thread_local! {
    static EXPIRY: Cell<Option<(Instant, &'static AtomicBool)>> = const { Cell::new(None) };
}

pub(crate) struct OriginalMappingExpiry;
pub(crate) fn arm_original_mapping_expiry(
    deadline: Instant,
    hit: &'static AtomicBool,
) -> OriginalMappingExpiry {
    EXPIRY.with(|slot| assert!(slot.replace(Some((deadline, hit))).is_none()));
    OriginalMappingExpiry
}
impl Drop for OriginalMappingExpiry {
    fn drop(&mut self) {
        EXPIRY.with(|slot| slot.set(None));
    }
}
pub(super) fn wait_for_original_deadline() {
    if let Some((deadline, hit)) = EXPIRY.with(Cell::take) {
        hit.store(true, Ordering::Release);
        while Instant::now() < deadline {
            std::thread::park_timeout(deadline.saturating_duration_since(Instant::now()));
        }
    }
}
