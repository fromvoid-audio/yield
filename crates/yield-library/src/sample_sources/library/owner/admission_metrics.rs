//! Bounded acquisition-local diagnostics; never durable state or command authority.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use super::GlobalLibraryWriterQueueError;

/// Finite queue-admission observations for one global owner acquisition.
///
/// Fields saturate at `u64::MAX`. Concurrent snapshots are approximate across fields.
/// Input-validation rejections are excluded. No count represents queue depth, SQL
/// completion, durable disposition, or a capacity reservation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GlobalAdmissionMetricsSnapshot {
    /// Requests successfully submitted to the bounded owner queue.
    pub accepted: u64,
    /// Attempts rejected because the bounded owner queue was full.
    pub full: u64,
    /// Attempts rejected before owner initialization completed.
    pub initializing: u64,
    /// Attempts rejected because the owner was unavailable, without exporting its reason.
    pub unavailable: u64,
    /// Attempts rejected after admission closed or the receiver disconnected.
    pub closed: u64,
    /// Admission-gate wait counts: <1ms, [1,10)ms, [10,100)ms, [100,1000)ms, >=1s.
    /// Lifecycle rejection before the gate produces no wait observation. This measures
    /// mutex acquisition only, never queue residence, SQLite wait, or execution time.
    pub gate_wait_buckets: [u64; 5],
}

#[derive(Default)]
pub(super) struct AdmissionMetrics {
    pub(super) dispatch: std::sync::Arc<super::dispatch_metrics::DispatchMetrics>,
    outcomes: [AtomicU64; 5],
    waits: [AtomicU64; 5],
}

fn increment(counter: &AtomicU64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        Some(value.saturating_add(1))
    });
}

fn wait_bucket(wait: Duration) -> usize {
    match wait.as_millis() {
        0 => 0,
        1..=9 => 1,
        10..=99 => 2,
        100..=999 => 3,
        _ => 4,
    }
}

impl AdmissionMetrics {
    pub(super) fn record_outcome(&self, result: &Result<(), GlobalLibraryWriterQueueError>) {
        let (index, outcome) = match result {
            Ok(()) => (0, "accepted"),
            Err(GlobalLibraryWriterQueueError::Full) => (1, "full"),
            Err(GlobalLibraryWriterQueueError::Initializing) => (2, "initializing"),
            Err(GlobalLibraryWriterQueueError::Unavailable(_)) => (3, "unavailable"),
            Err(GlobalLibraryWriterQueueError::Closed) => (4, "closed"),
            // Input/route/bound validation is outside the queue-admission boundary.
            Err(_) => return,
        };
        increment(&self.outcomes[index]);
        tracing::debug!(target: "yield::global_owner::admission", schema_version = 1_u8,
            owner = "global_library", admission_outcome = outcome,
            "Global owner admission observed");
    }

    pub(super) fn record_wait(&self, wait: Duration) {
        let bucket = wait_bucket(wait);
        increment(&self.waits[bucket]);
        tracing::debug!(target: "yield::global_owner::admission", schema_version = 1_u8,
            owner = "global_library", gate_wait_bucket = bucket as u8,
            "Global owner admission gate wait observed");
    }

    pub(super) fn snapshot(&self) -> GlobalAdmissionMetricsSnapshot {
        let [accepted, full, initializing, unavailable, closed] = self
            .outcomes
            .each_ref()
            .map(|counter| counter.load(Ordering::Relaxed));
        GlobalAdmissionMetricsSnapshot {
            accepted,
            full,
            initializing,
            unavailable,
            closed,
            gate_wait_buckets: self
                .waits
                .each_ref()
                .map(|counter| counter.load(Ordering::Relaxed)),
        }
    }
}

#[cfg(test)]
#[path = "admission_metrics_tests.rs"]
mod tests;
