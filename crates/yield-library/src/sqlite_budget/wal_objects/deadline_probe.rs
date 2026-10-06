//! One-shot original-worker scheduling gate. No observation or authority is fabricated.
use std::cell::Cell;
use std::time::Instant;

thread_local! {
    static EXPIRY: Cell<Option<Instant>> = const { Cell::new(None) };
}

pub(crate) struct OriginalMappingExpiry;
pub(crate) fn arm_original_mapping_expiry(deadline: Instant) -> OriginalMappingExpiry {
    EXPIRY.with(|slot| assert!(slot.replace(Some(deadline)).is_none()));
    OriginalMappingExpiry
}
impl Drop for OriginalMappingExpiry {
    fn drop(&mut self) {
        EXPIRY.with(|slot| slot.set(None));
    }
}
pub(super) fn wait_for_original_deadline() {
    if let Some(deadline) = EXPIRY.with(Cell::take) {
        while Instant::now() < deadline {
            std::thread::park_timeout(deadline.saturating_duration_since(Instant::now()));
        }
        eprintln!("OPT2404 original native mapping observer reached its unchanged deadline");
    }
}
