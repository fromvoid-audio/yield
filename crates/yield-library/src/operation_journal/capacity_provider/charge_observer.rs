//! Bounded scalar observations of the original actor ledger, never entry authority.
use super::retained_backing::RetainedWeak;
use super::*;
use std::sync::{Arc, Mutex};

const MAX_VOLUMES: usize = 32;
#[derive(Debug)]
pub(super) struct ChargeMirror(
    Mutex<State>,
    #[cfg(test)] Mutex<Option<Arc<std::sync::Barrier>>>,
);
#[derive(Debug)]
struct State {
    generation: u64,
    pending: bool,
    closed: bool,
    volumes: [Option<BoundVolume>; MAX_VOLUMES],
}
#[derive(Debug)]
struct BoundVolume {
    observer: RetainedWeak<ChargeObserver>,
    total: u64,
}
/// Only the original provider can bind this scalar observer to an actual charged volume.
/// It contains no ledger, native resource, capacity qualification or effect permission.
#[derive(Debug)]
pub(crate) struct ChargeObserver {
    mirror: RetainedArc<ChargeMirror>,
    volume: VolumeIdentity,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)] // Scalar consumer installed by full OPT-2215.
pub(crate) struct ChargeObservation {
    pub(crate) generation: u64,
    pub(crate) incremental_bytes: u64,
}

/// Exact durable charge observation, minted by the original actor-owned provider only.
/// No actor ledger, native handle, effect permission or settlement authority is retained.
pub(crate) struct PublicationChargeBinding {
    operation: OperationId,
    claim: AdmissionClaim,
    observer: RetainedArc<ChargeObserver>,
    observation: ChargeObservation,
}
// The future slot/input producer must debit this retained header on its original credit.
const _: () = assert!(std::mem::size_of::<PublicationChargeBinding>() <= 256);
impl PublicationChargeBinding {
    pub(super) fn new(
        operation: OperationId,
        claim: AdmissionClaim,
        observer: RetainedArc<ChargeObserver>,
        observation: ChargeObservation,
    ) -> Self {
        Self {
            operation,
            claim,
            observer,
            observation,
        }
    }
    pub(super) fn belongs_to(&self, mirror: &RetainedArc<ChargeMirror>) -> bool {
        RetainedArc::ptr_eq(&self.observer.mirror, mirror)
    }
    pub(super) fn operation(&self) -> OperationId {
        self.operation
    }
    pub(crate) fn claim(&self) -> &AdmissionClaim {
        &self.claim
    }
    /// Historical scalar validity only. The issuer still needs independent admission,
    /// source, target, native facts, namespace allocation policy and lifetime fences.
    pub(crate) fn is_current(&self) -> bool {
        self.with_current(|_| ()).is_some()
    }
    /// Run a scalar-only transition while the exact captured generation stays current.
    /// This provides neither an effect gate nor a replacement for actor revalidation.
    pub(crate) fn with_current<T>(
        &self,
        transition: impl FnOnce(ChargeObservation) -> T,
    ) -> Option<T> {
        self.observer
            .with_current(Some(self.observation), transition)
    }
}
// Fixed registry and all thirty-two unique observer allocations fit a bounded header.
// The full issuer must still debit this backing as part of its original slot/input credit.
const _: () = assert!(
    std::mem::size_of::<ChargeMirror>()
        + MAX_VOLUMES * (std::mem::size_of::<ChargeObserver>() + 2 * std::mem::size_of::<usize>())
        <= super::PUBLICATION_MIRROR_BYTES
);
impl Default for ChargeMirror {
    fn default() -> Self {
        Self(
            Mutex::new(State {
                generation: 1,
                pending: false,
                closed: false,
                volumes: std::array::from_fn(|_| None),
            }),
            #[cfg(test)]
            Mutex::new(None),
        )
    }
}
fn total(ledger: &ProvisionalLedger, volume: &VolumeIdentity) -> Option<u64> {
    ledger
        .aggregate_charges
        .iter()
        .filter(|(key, _)| {
            key.volume == *volume && counts_against_incremental_free_space(key.allocation_class)
        })
        .try_fold(0u64, |sum, (_, bytes)| sum.checked_add(*bytes))
}
impl ChargeMirror {
    pub(super) fn close(&self) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        state.closed = true;
    }
    pub(super) fn bind(
        mirror: &RetainedArc<Self>,
        ledger: &ProvisionalLedger,
        volume: &VolumeIdentity,
    ) -> Result<RetainedArc<ChargeObserver>, CapacityProviderError> {
        let mut state = mirror
            .0
            .lock()
            .map_err(|_| CapacityProviderError::Unavailable)?;
        if state.closed || state.pending {
            return Err(CapacityProviderError::Unavailable);
        }
        for bound in state.volumes.iter().flatten() {
            if let Some(observer) = bound.observer.upgrade()
                && observer.volume == *volume
            {
                return Ok(observer);
            }
        }
        if !ledger
            .aggregate_charges
            .keys()
            .any(|key| key.volume == *volume)
        {
            return Err(CapacityProviderError::UnsupportedFacts);
        }
        let Some(index) = state
            .volumes
            .iter()
            .position(|v| v.as_ref().is_none_or(|v| v.observer.strong_count() == 0))
        else {
            return Err(CapacityProviderError::ClaimInventoryExhausted);
        };
        let Some(total) = total(ledger, volume) else {
            state.closed = true;
            return Err(CapacityProviderError::ArithmeticOverflow);
        };
        let observer = RetainedArc::new(
            ChargeObserver {
                mirror: mirror.clone(),
                volume: volume.clone(),
            },
            mirror.credit(),
        );
        state.volumes[index] = Some(BoundVolume {
            observer: observer.downgrade(),
            total,
        });
        Ok(observer)
    }
}
#[allow(dead_code)] // No standalone observer authorizes publication entry.
impl ChargeObserver {
    /// Historical read only. Final entry must also exclude original-ledger mutation.
    pub(crate) fn observation(&self) -> Option<ChargeObservation> {
        self.with_current(None, |observation| observation)
    }
    /// Execute only a scalar consumer transition while this exact generation stays current.
    /// No SQL, native calls, callbacks or waits may run inside the transition. This method
    /// grants no entry authority; the future issuer must hold its independent admission gate.
    pub(crate) fn with_current<T>(
        &self,
        expected: Option<ChargeObservation>,
        transition: impl FnOnce(ChargeObservation) -> T,
    ) -> Option<T> {
        let state = self.mirror.0.lock().ok()?;
        if state.closed || state.pending {
            return None;
        }
        let bound = state
            .volumes
            .iter()
            .flatten()
            .find(|bound| std::ptr::eq(bound.observer.as_ptr(), self))?;
        let current = ChargeObservation {
            generation: state.generation,
            incremental_bytes: bound.total,
        };
        if expected.is_some_and(|expected| expected != current) {
            return None;
        }
        Some(transition(current))
    }
}
/// Pending begins before any ledger write. Unconfirmed completion or unwind is sticky closed.
#[must_use]
pub(super) struct ChargeMutation {
    mirror: RetainedArc<ChargeMirror>,
    completed: bool,
}
impl ChargeMutation {
    pub(super) fn begin(ledger: &ProvisionalLedger) -> Self {
        let mirror = ledger.mirror.clone();
        match mirror.0.lock() {
            Ok(mut state) => {
                if state.pending {
                    state.closed = true;
                }
                state.pending = true;
            }
            Err(poisoned) => {
                poisoned.into_inner().closed = true;
            }
        }
        #[cfg(test)]
        if let Some(barrier) = mirror.1.lock().unwrap().take() {
            barrier.wait();
            barrier.wait();
        }
        Self {
            mirror,
            completed: false,
        }
    }
    pub(super) fn finish(mut self, ledger: &ProvisionalLedger) {
        let mut state = self.mirror.0.lock().unwrap_or_else(|p| p.into_inner());
        // Poison is permanent, even if the original ledger can still retain/release charges.
        if self.mirror.0.is_poisoned() {
            state.closed = true;
        }
        if !state.closed {
            let next = state.generation.checked_add(1);
            let mut totals = [0u64; MAX_VOLUMES];
            let mut overflow = false;
            for (index, bound) in state.volumes.iter().enumerate() {
                if let Some(observer) = bound.as_ref().and_then(|bound| bound.observer.upgrade()) {
                    match total(ledger, &observer.volume) {
                        Some(total) => totals[index] = total,
                        None => {
                            overflow = true;
                            break;
                        }
                    }
                }
            }
            if let Some(next) = next.filter(|_| !overflow) {
                for (index, bound) in state.volumes.iter_mut().enumerate() {
                    if let Some(bound) = bound {
                        bound.total = totals[index];
                    }
                }
                state.generation = next;
                state.pending = false;
            } else {
                state.closed = true;
            }
        }
        self.completed = true;
    }
}
impl Drop for ChargeMutation {
    fn drop(&mut self) {
        if !self.completed {
            self.mirror.close();
        }
    }
}
#[cfg(test)]
mod exact_binding_tests;
#[cfg(test)]
mod tests;
