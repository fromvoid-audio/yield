//! Fixed original provisional-ledger return observations, never physical capacity authority.
use super::capacity_provider::CapacityProviderError;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Fixed category of an original provider admission return.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
pub enum CapacityAdmissionDecision {
    /// Returned the existing logical provisional lease, before durable settlement.
    ProvisionalLease,
    /// The original provider fallback is unavailable.
    Unavailable,
    /// Plan or claim validation refused the request.
    InvalidInput,
    /// A required volume observation is missing.
    MissingFacts,
    /// Facts or allocation units are unsupported.
    UnsupportedFacts,
    /// Free space is at or below the protected floor.
    ProtectedFloor,
    /// Existing plus proposed charges exceed observed headroom.
    Pressure,
    /// Checked arithmetic refused the request.
    Overflow,
    /// The original bounded claim or token inventory is exhausted.
    Exhausted,
    /// The original owner-thread ledger cannot be borrowed.
    Busy,
    /// Unresolved persistence prevents admission.
    RecoveryRequired,
    /// Another original provider error, without exposing its payload.
    OtherRefusal,
}
const DECISIONS: usize = 12;
/// Fixed process-lifetime counters. Fields are sampled independently, not a coherent ledger.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapacityAdmissionTelemetrySnapshot {
    counts: [u64; DECISIONS],
    /// Call entry to selected return, <=1, <=10, <=100, <=1000 and >1000 ms.
    pub elapsed_buckets: [u64; 5],
}
impl CapacityAdmissionTelemetrySnapshot {
    /// Observed returns in one fixed category; each counter saturates independently.
    pub fn count(&self, decision: CapacityAdmissionDecision) -> u64 {
        self.counts[decision as usize]
    }
}
struct Counters {
    counts: [AtomicU64; DECISIONS],
    elapsed: [AtomicU64; 5],
}
impl Counters {
    const fn new() -> Self {
        Self {
            counts: [const { AtomicU64::new(0) }; DECISIONS],
            elapsed: [const { AtomicU64::new(0) }; 5],
        }
    }
    fn record(&self, decision: CapacityAdmissionDecision, elapsed: Duration) {
        let increment = |counter: &AtomicU64| {
            let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some(n.saturating_add(1))
            });
        };
        increment(&self.counts[decision as usize]);
        let bucket = [1, 10, 100, 1000]
            .iter()
            .position(|ms| elapsed <= Duration::from_millis(*ms))
            .unwrap_or(4);
        increment(&self.elapsed[bucket]);
    }
    fn snapshot(&self) -> CapacityAdmissionTelemetrySnapshot {
        CapacityAdmissionTelemetrySnapshot {
            counts: std::array::from_fn(|i| self.counts[i].load(Ordering::Relaxed)),
            elapsed_buckets: std::array::from_fn(|i| self.elapsed[i].load(Ordering::Relaxed)),
        }
    }
}
static COUNTERS: Counters = Counters::new();
/// Sample numeric observations without reading owners, files, SQL or ledger identities.
pub fn capacity_admission_telemetry_snapshot() -> CapacityAdmissionTelemetrySnapshot {
    COUNTERS.snapshot()
}
fn classify<T>(result: &Result<T, CapacityProviderError>) -> CapacityAdmissionDecision {
    use CapacityAdmissionDecision as D;
    use CapacityProviderError as E;
    match result {
        Ok(_) => D::ProvisionalLease,
        Err(E::Unavailable) => D::Unavailable,
        Err(E::InvalidPlan | E::InvalidClaims) => D::InvalidInput,
        Err(E::FactsUnavailable { .. }) => D::MissingFacts,
        Err(E::UnsupportedFacts | E::UnsupportedAllocationUnit { .. }) => D::UnsupportedFacts,
        Err(E::RecoveryReserveLow { .. }) => D::ProtectedFloor,
        Err(E::DiskPressureRecoveryOnly { .. }) => D::Pressure,
        Err(E::ArithmeticOverflow) => D::Overflow,
        Err(E::ClaimInventoryExhausted | E::LeaseTokenExhausted) => D::Exhausted,
        Err(E::LedgerBusy) => D::Busy,
        Err(E::RecoveryRequired) => D::RecoveryRequired,
        Err(_) => D::OtherRefusal,
    }
}
pub(super) fn observe<T>(
    call: impl FnOnce() -> Result<T, CapacityProviderError>,
) -> Result<T, CapacityProviderError> {
    let started = Instant::now();
    let result = call();
    let elapsed = started.elapsed();
    let decision = classify(&result);
    COUNTERS.record(decision, elapsed);
    #[cfg(test)]
    tests::record(decision, elapsed);
    result
}
#[cfg(test)]
pub(super) mod tests;
