//! Fixed original ledger return observations; no retry, refund or settlement authority.
use super::capacity_provider::CapacityProviderError;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Existing primitive call being observed. Nested original calls keep their own identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
pub enum CapacityLedgerCall {
    /// Existing provisional release, including the existing Drop attempt.
    ProvisionalRelease,
    /// Bind or confirm an audit-pending disposition.
    AuditPendingBinding,
    /// Bind or confirm a durable disposition.
    DurableBinding,
    /// Caller requests the existing confirmed-rollback release.
    ConfirmedRollbackRelease,
    /// Existing exact ordinary durable claim release or confirmation.
    OrdinaryClaimRelease,
}
/// Original returned result, never a new transition or completed genuine settlement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
pub enum CapacityLedgerReturn {
    /// Original success, including idempotent confirmation.
    Confirmed,
    /// Original durable/audit-pending retention refusal.
    Retained,
    /// Original owner-thread ledger borrow refusal.
    Busy,
    /// Any other original refusal, without its payload.
    Refused,
}
/// Independent process-lifetime atomic samples, not a coherent ledger or live gauge.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapacityRetentionTelemetrySnapshot {
    counts: [[u64; 4]; 5],
    /// Primitive entry through original return: <=1, <=10, <=100, <=1000, >1000 ms.
    pub elapsed_buckets: [u64; 5],
}
impl CapacityRetentionTelemetrySnapshot {
    /// Saturating observations for one original primitive/result category.
    pub fn count(&self, call: CapacityLedgerCall, result: CapacityLedgerReturn) -> u64 {
        self.counts[call as usize][result as usize]
    }
}
struct Counters {
    counts: [[AtomicU64; 4]; 5],
    elapsed: [AtomicU64; 5],
}
impl Counters {
    const fn new() -> Self {
        Self {
            counts: [const { [const { AtomicU64::new(0) }; 4] }; 5],
            elapsed: [const { AtomicU64::new(0) }; 5],
        }
    }
    fn record(&self, call: CapacityLedgerCall, result: CapacityLedgerReturn, elapsed: Duration) {
        let increment = |counter: &AtomicU64| {
            let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some(n.saturating_add(1))
            });
        };
        increment(&self.counts[call as usize][result as usize]);
        let bucket = [1, 10, 100, 1000]
            .iter()
            .position(|ms| elapsed <= Duration::from_millis(*ms))
            .unwrap_or(4);
        increment(&self.elapsed[bucket]);
    }
    fn snapshot(&self) -> CapacityRetentionTelemetrySnapshot {
        CapacityRetentionTelemetrySnapshot {
            counts: std::array::from_fn(|i| {
                std::array::from_fn(|j| self.counts[i][j].load(Ordering::Relaxed))
            }),
            elapsed_buckets: std::array::from_fn(|i| self.elapsed[i].load(Ordering::Relaxed)),
        }
    }
}
static COUNTERS: Counters = Counters::new();
/// Read fixed numeric samples without reading any owner, ledger, SQL, file or identity.
pub fn capacity_retention_telemetry_snapshot() -> CapacityRetentionTelemetrySnapshot {
    COUNTERS.snapshot()
}
pub(super) fn observe<T>(
    call: CapacityLedgerCall,
    invoke: impl FnOnce() -> Result<T, CapacityProviderError>,
) -> Result<T, CapacityProviderError> {
    let started = Instant::now();
    let result = invoke();
    let elapsed = started.elapsed();
    let category = match &result {
        Ok(_) => CapacityLedgerReturn::Confirmed,
        Err(CapacityProviderError::LeaseRetained) => CapacityLedgerReturn::Retained,
        Err(CapacityProviderError::LedgerBusy) => CapacityLedgerReturn::Busy,
        Err(_) => CapacityLedgerReturn::Refused,
    };
    COUNTERS.record(call, category, elapsed);
    #[cfg(test)]
    tests::record(call, category, elapsed);
    result
}
#[cfg(test)]
pub(super) mod tests;
