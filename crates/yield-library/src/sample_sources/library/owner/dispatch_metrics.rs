//! Fixed acquisition-local observations of actual retained-worker dequeue.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use super::WriterCommand;

/// Cumulative diagnostics for one retained global owner acquisition.
///
/// Fields saturate; concurrent snapshots are approximate across fields. These
/// observations establish neither queue depth nor SQL execution/commit success.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GlobalDispatchMetricsSnapshot {
    /// Commands dequeued by the normal worker and passing its profile check.
    pub dequeued: u64,
    /// Submission through actual dequeue: <1ms, [1,10)ms, [10,100)ms,
    /// [100,1000)ms, >=1s. Includes try_send/scheduling, excludes gate/SQL time.
    pub wait_buckets: [u64; 5],
}

#[derive(Default)]
pub(super) struct DispatchMetrics {
    dequeued: AtomicU64,
    waits: [AtomicU64; 5],
}

fn increment(counter: &AtomicU64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        Some(value.saturating_add(1))
    });
}

impl DispatchMetrics {
    fn record(&self, wait: Duration) {
        increment(&self.dequeued);
        let bucket = match wait.as_millis() {
            0 => 0,
            1..=9 => 1,
            10..=99 => 2,
            100..=999 => 3,
            _ => 4,
        };
        increment(&self.waits[bucket]);
    }

    pub(super) fn snapshot(&self) -> GlobalDispatchMetricsSnapshot {
        GlobalDispatchMetricsSnapshot {
            dequeued: self.dequeued.load(Ordering::Relaxed),
            wait_buckets: self
                .waits
                .each_ref()
                .map(|counter| counter.load(Ordering::Relaxed)),
        }
    }
}

/// Private bounded queue metadata, not a serialized command or live authority.
pub(super) struct QueuedCommand {
    command: WriterCommand,
    submitted: Instant,
    metrics: Arc<DispatchMetrics>,
}

impl QueuedCommand {
    pub(super) fn new(command: WriterCommand, metrics: Arc<DispatchMetrics>) -> Self {
        Self {
            command,
            metrics,
            submitted: Instant::now(),
        }
    }

    pub(super) fn received(self) -> (WriterCommand, PendingDispatch) {
        let wait = self.submitted.elapsed();
        (
            self.command,
            PendingDispatch {
                metrics: self.metrics,
                wait,
            },
        )
    }

    /// Profile-loss draining/discard does not become normal dispatch evidence.
    pub(super) fn discard(self) -> WriterCommand {
        self.command
    }
}

pub(super) struct PendingDispatch {
    metrics: Arc<DispatchMetrics>,
    wait: Duration,
}

impl PendingDispatch {
    /// Record the already captured dequeue instant only after profile revalidation.
    pub(super) fn record(self) {
        self.metrics.record(self.wait);
    }
}

#[cfg(test)]
mod tests;
