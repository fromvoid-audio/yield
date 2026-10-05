//! Fixed observations of the actual existing source reservation boundary.
use super::{MAX_ATTEMPTS, Observation, SourceDbError};
use rusqlite::Transaction;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Cumulative process-wide ordinary source writer acquisition observations.
/// Fields saturate; concurrent snapshots are approximate across fields.
/// No source identities, errors, requests or authority are retained.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SourceAcquisitionMetricsSnapshot {
    /// Actual BEGIN IMMEDIATE calls, including failed attempts.
    pub attempts: u64,
    /// Pre-effect retry waits scheduled by the existing policy.
    pub retries: u64,
    /// Reservations returned after restoring the original timeout, before any body.
    pub acquired: u64,
    /// Plain BUSY returned after the final permitted attempt.
    pub busy_exhausted: u64,
    /// Other BUSY/LOCKED returned without replaying a transaction.
    pub contention_not_retried: u64,
    /// Calls rejected because a transaction was already active.
    pub nested_transaction: u64,
    /// Profile/lease or retained ownership revalidation refusals.
    pub ownership_rejected: u64,
    /// Other setup/SQLite/timeout restoration failures.
    pub other_failure: u64,
    /// Invocation through timeout restoration/result selection: <1ms, [1,10)ms,
    /// [10,100)ms, [100,1000)ms, >=1s. Excludes trace delivery and body/commit.
    pub elapsed_buckets: [u64; 5],
}

/// Read numeric observations; this does not authorize admission, retries or effects.
pub fn source_acquisition_metrics_snapshot() -> SourceAcquisitionMetricsSnapshot {
    PROCESS_METRICS.snapshot()
}

pub(super) static PROCESS_METRICS: AcquisitionMetrics = AcquisitionMetrics::new();
pub(super) struct AcquisitionMetrics {
    counts: [AtomicU64; 8],
    elapsed: [AtomicU64; 5],
}
fn increment(counter: &AtomicU64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        Some(value.saturating_add(1))
    });
}
impl AcquisitionMetrics {
    pub(super) const fn new() -> Self {
        Self {
            counts: [const { AtomicU64::new(0) }; 8],
            elapsed: [const { AtomicU64::new(0) }; 5],
        }
    }
    pub(super) fn attempt(&self) {
        increment(&self.counts[0]);
    }
    pub(super) fn retry(&self) {
        increment(&self.counts[1]);
    }
    pub(super) fn finish(
        &self,
        result: &Result<Transaction<'_>, SourceDbError>,
        nested: bool,
        observation: &Observation,
        elapsed: Duration,
    ) {
        let outcome = match result {
            Ok(_) => 2,
            Err(_) if nested => 5,
            Err(
                SourceDbError::ProfileOwnershipRequired
                | SourceDbError::ProfileOwnership(_)
                | SourceDbError::SourceLease(_),
            ) => 6,
            Err(_) if observation.outcome == "ownership_rejected" => 6,
            Err(SourceDbError::Busy) if observation.attempts == MAX_ATTEMPTS => 3,
            Err(_) if observation.outcome == "contention_not_retryable" => 4,
            Err(_) => 7,
        };
        increment(&self.counts[outcome]);
        let bucket = match elapsed.as_millis() {
            0 => 0,
            1..=9 => 1,
            10..=99 => 2,
            100..=999 => 3,
            _ => 4,
        };
        increment(&self.elapsed[bucket]);
    }
    pub(super) fn snapshot(&self) -> SourceAcquisitionMetricsSnapshot {
        let [
            attempts,
            retries,
            acquired,
            busy_exhausted,
            contention_not_retried,
            nested_transaction,
            ownership_rejected,
            other_failure,
        ] = self.counts.each_ref().map(|c| c.load(Ordering::Relaxed));
        SourceAcquisitionMetricsSnapshot {
            attempts,
            retries,
            acquired,
            busy_exhausted,
            contention_not_retried,
            nested_transaction,
            ownership_rejected,
            other_failure,
            elapsed_buckets: self.elapsed.each_ref().map(|c| c.load(Ordering::Relaxed)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixed_storage_saturation_and_bucket_edges() {
        let metrics = AcquisitionMetrics::new();
        for counter in metrics.counts.iter().chain(&metrics.elapsed) {
            counter.store(u64::MAX - 1, Ordering::Relaxed);
            increment(counter);
            increment(counter);
            assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
        }
        let metrics = AcquisitionMetrics::new();
        for millis in [0, 1, 9, 10, 99, 100, 999, 1000, u64::MAX] {
            metrics.finish(
                &Err(SourceDbError::Unexpected),
                true,
                &Observation::default(),
                Duration::from_millis(millis),
            );
        }
        assert_eq!(metrics.snapshot().elapsed_buckets, [1, 2, 2, 2, 2]);
        assert_eq!(metrics.snapshot().nested_transaction, 9);
        assert_eq!(
            std::mem::size_of::<SourceAcquisitionMetricsSnapshot>(),
            13 * 8
        );
        assert_eq!(std::mem::size_of::<AcquisitionMetrics>(), 13 * 8);
    }
}
