//! Free the original reply Arc allocation before releasing its embedded header credit.
use super::HistoricalReplyBacking;
use std::ops::Deref;
use std::sync::Arc;

// The Arc is private to this module. No raw Arc or Weak can escape, and every
// strong alias must run this Drop implementation, including concurrent last drops.
#[derive(Clone)]
pub(super) struct HistoricalReply(Option<Arc<HistoricalReplyBacking>>);

impl HistoricalReply {
    pub(super) fn new(backing: HistoricalReplyBacking) -> Self {
        Self(Some(Arc::new(backing)))
    }
}
impl Deref for HistoricalReply {
    type Target = HistoricalReplyBacking;
    fn deref(&self) -> &Self::Target {
        self.0.as_deref().expect("live historical reply alias")
    }
}
impl Drop for HistoricalReply {
    fn drop(&mut self) {
        if let Some(arc) = self.0.take() {
            // Pinned Rust 1.93 alloc/src/sync.rs into_inner moves T, then drops
            // the implicit Weak before returning T. With no external Weak, that
            // frees the Arc allocation first. Every alias using into_inner also
            // guarantees exactly one winner under concurrent last-alias drops.
            // Finally drop working storage, then Backing's LAST header permit.
            drop(Arc::into_inner(arc));
        }
    }
}
