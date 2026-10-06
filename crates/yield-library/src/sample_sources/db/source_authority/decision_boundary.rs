//! Original registration comparison custody, never an effect or settlement grant.
use super::*;

#[derive(Clone)]
pub(crate) struct RegisteredDecisionBoundary(Arc<AuthorityState>);
const _: () = assert!(std::mem::size_of::<RegisteredDecisionBoundary>() <= 16);

impl SourceAuthorityRegistration {
    pub(crate) fn decision_boundary(&self) -> Option<RegisteredDecisionBoundary> {
        let backing = std::mem::size_of::<AuthorityState>()
            .checked_add(self.request.state.lease_identity.owner_identity.capacity())?
            .checked_add(std::mem::size_of::<SourceEffectGate>())?
            .checked_add(std::mem::size_of::<AtomicBool>() + 3 * 32)?;
        (backing <= RegisteredDecisionBoundary::MAX_BACKING_BYTES)
            .then(|| RegisteredDecisionBoundary(Arc::clone(&self.request.state)))
    }
}
impl RegisteredDecisionBoundary {
    // State, its UUID-owner string, shared gate/liveness and conservative Arc headers.
    pub(crate) const MAX_BACKING_BYTES: usize = 512;
    pub(crate) fn same_gate(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0.effect_gate, &other.0.effect_gate)
    }
    pub(crate) fn registration(&self) -> Uuid {
        self.0.registration_id.0
    }

    /// Runs only bounded scalar work while drain/abandon compete on this same gate.
    /// Releasing this lock establishes no freshness at a later SQL or effect boundary.
    pub(crate) fn with_current<T>(&self, compare: impl FnOnce() -> T) -> Option<T> {
        let gate = self.0.effect_gate.state.lock().ok()?;
        if !gate.open
            || self.0.phase() != REGISTERED
            || !self.0.lease_liveness.load(Ordering::Acquire)
        {
            return None;
        }
        Some(compare())
    }
}
