//! Fixed process observations of acquisition, never transaction or capacity authority.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rusqlite::{Transaction, ffi};

use super::{LibraryError, MAX_ATTEMPTS};

/// Process-wide cumulative ordinary global SQLite acquisition observations.
///
/// All fields saturate. Concurrent reads are approximate across fields; counters
/// cannot reconstruct requests or establish admission, durability, or capacity.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GlobalAcquisitionMetricsSnapshot {
    /// Actual BEGIN IMMEDIATE invocations, including unsuccessful ones.
    pub attempts: u64,
    /// Pre-effect retries scheduled by the unchanged acquisition policy.
    pub retries: u64,
    /// Successful writer reservations after timeout restoration, before any body.
    pub acquired: u64,
    /// Plain BUSY returned after the final permitted attempt.
    pub busy_exhausted: u64,
    /// Other BUSY/LOCKED dispositions, preserving exact codes only in caller errors.
    pub contention_not_retried: u64,
    /// Calls rejected because the caller already owned an active transaction.
    pub nested_transaction: u64,
    /// Other failures, including timeout inspection/configuration/restoration errors.
    pub other_failure: u64,
    /// Invocation through restoration/return: <1ms, [1,10)ms, [10,100)ms,
    /// [100,1000)ms, >=1s. No individual timestamps or durations are retained.
    pub elapsed_buckets: [u64; 5],
}

/// Read cumulative diagnostics without resetting them or observing SQL contents.
pub fn global_acquisition_metrics_snapshot() -> GlobalAcquisitionMetricsSnapshot {
    PROCESS_METRICS.snapshot()
}

pub(super) static PROCESS_METRICS: AcquisitionMetrics = AcquisitionMetrics::new();

pub(super) struct AcquisitionMetrics {
    counts: [AtomicU64; 7],
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
            counts: [const { AtomicU64::new(0) }; 7],
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
        result: &Result<Transaction<'_>, LibraryError>,
        nested: bool,
        elapsed: Duration,
    ) {
        let outcome = match result {
            Ok(_) => 2,
            Err(_) if nested => 5,
            Err(LibraryError::OrdinaryWriteContention {
                attempts,
                sqlite_code,
            }) if *attempts == MAX_ATTEMPTS && *sqlite_code == ffi::SQLITE_BUSY => 3,
            Err(LibraryError::OrdinaryWriteContention { .. }) => 4,
            Err(_) => 6,
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

    pub(super) fn snapshot(&self) -> GlobalAcquisitionMetricsSnapshot {
        let [
            attempts,
            retries,
            acquired,
            busy_exhausted,
            contention_not_retried,
            nested_transaction,
            other_failure,
        ] = self
            .counts
            .each_ref()
            .map(|counter| counter.load(Ordering::Relaxed));
        GlobalAcquisitionMetricsSnapshot {
            attempts,
            retries,
            acquired,
            busy_exhausted,
            contention_not_retried,
            nested_transaction,
            other_failure,
            elapsed_buckets: self
                .elapsed
                .each_ref()
                .map(|counter| counter.load(Ordering::Relaxed)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_counters_saturate_and_elapsed_boundaries_are_finite() {
        let metrics = AcquisitionMetrics::new();
        for counter in metrics.counts.iter().chain(&metrics.elapsed) {
            counter.store(u64::MAX - 1, Ordering::Relaxed);
            increment(counter);
            increment(counter);
            assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
        }
        let metrics = AcquisitionMetrics::new();
        let error = Err(LibraryError::OrdinaryWriteContention {
            attempts: 1,
            sqlite_code: ffi::SQLITE_LOCKED_SHAREDCACHE,
        });
        for millis in [0, 1, 9, 10, 99, 100, 999, 1000, u64::MAX] {
            metrics.finish(&error, false, Duration::from_millis(millis));
        }
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.elapsed_buckets, [1, 2, 2, 2, 2]);
        assert_eq!(snapshot.contention_not_retried, 9);
        // Fixed numeric storage; neither inputs nor error payloads are retained.
        assert_eq!(std::mem::size_of_val(&snapshot), 12 * 8);
        assert_eq!(std::mem::size_of::<AcquisitionMetrics>(), 12 * 8);
    }
}
