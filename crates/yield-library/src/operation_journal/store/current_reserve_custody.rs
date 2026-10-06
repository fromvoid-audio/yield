//! Keep an original opened reserve in the preinstalled recovery sink during unwinding.
//! These stack guards allocate no backing and confer no authority or clean handoff.
use super::{BoundReserve, OwnedReserveAllocation, ReserveBootstrapProof};
use std::ops::{Deref, DerefMut};

pub(super) struct OwnedCustody<'a, 'p, 's> {
    reserve: Option<OwnedReserveAllocation>,
    proof: &'a ReserveBootstrapProof<'p, 's>,
}
impl<'a, 'p, 's> OwnedCustody<'a, 'p, 's> {
    pub(super) fn new(
        reserve: OwnedReserveAllocation,
        proof: &'a ReserveBootstrapProof<'p, 's>,
    ) -> Self {
        Self {
            reserve: Some(reserve),
            proof,
        }
    }
    pub(super) fn take(mut self) -> OwnedReserveAllocation {
        self.reserve
            .take()
            .expect("original reserve transfers once")
    }
}
impl Deref for OwnedCustody<'_, '_, '_> {
    type Target = OwnedReserveAllocation;
    fn deref(&self) -> &Self::Target {
        self.reserve
            .as_ref()
            .expect("original reserve remains held")
    }
}
impl DerefMut for OwnedCustody<'_, '_, '_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.reserve
            .as_mut()
            .expect("original reserve remains held")
    }
}
impl Drop for OwnedCustody<'_, '_, '_> {
    fn drop(&mut self) {
        if let Some(reserve) = self.reserve.take() {
            // Original Current/Legacy sinks are installed before opening the child. Other
            // recovery consumers keep their existing descriptor drop behavior.
            self.proof.retain_reopened(reserve);
        }
    }
}

pub(super) struct BoundCustody<'a, 'r, 'p, 's> {
    reserve: Option<BoundReserve<'r>>,
    proof: Option<&'a ReserveBootstrapProof<'p, 's>>,
}
impl<'a, 'r, 'p, 's> BoundCustody<'a, 'r, 'p, 's> {
    pub(super) fn new(
        reserve: BoundReserve<'r>,
        proof: Option<&'a ReserveBootstrapProof<'p, 's>>,
    ) -> Self {
        Self {
            reserve: Some(reserve),
            proof,
        }
    }
    pub(super) fn take(mut self) -> BoundReserve<'r> {
        self.reserve
            .take()
            .expect("original bound reserve transfers once")
    }
}
impl<'r> Deref for BoundCustody<'_, 'r, '_, '_> {
    type Target = BoundReserve<'r>;
    fn deref(&self) -> &Self::Target {
        self.reserve
            .as_ref()
            .expect("original bound reserve remains held")
    }
}
impl Drop for BoundCustody<'_, '_, '_, '_> {
    fn drop(&mut self) {
        if let (Some(proof), Some(reserve)) = (self.proof, self.reserve.take()) {
            // A failed original root duplication itself returns the exact protected file in
            // the namespace failure capsule; retain_bound preserves that alternate backing.
            proof.retain_bound(reserve);
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::operation_journal) enum CurrentReserveBoundary {
    Created,
    CreatedBound,
    RecoveryReopened,
    CleanRebindOpened,
    CleanRebound,
}

#[cfg(test)]
pub(super) type CurrentReserveHook = Box<dyn FnMut(CurrentReserveBoundary, &std::fs::File) + Send>;

#[cfg(all(test, unix))]
#[path = "current_reserve_custody_tests.rs"]
mod tests;
