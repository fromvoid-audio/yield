//! Fixed acquisition-local observations of actual retained-worker dequeue.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{SyncSender, TrySendError},
};
use std::time::{Duration, Instant};

use super::WriterCommand;

/// Cumulative diagnostics for one retained global owner acquisition.
///
/// Fields saturate; concurrent snapshots are approximate across fields. These
/// observations establish neither SQL execution/commit success nor capacity authority.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GlobalDispatchMetricsSnapshot {
    /// Successfully enqueued envelopes awaiting dequeue accounting; excludes execution.
    /// Includes the short boundary after channel receive while its ticket takes the lock.
    pub queued: u64,
    /// Largest successful enqueue backlog in this acquisition.
    pub peak_queued: u64,
    /// Accepted envelopes explicitly discarded or dropped before dequeue accounting.
    pub abandoned: u64,
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
    queue: Mutex<QueueState>,
}

#[derive(Default)]
struct QueueState {
    queued: u64,
    peak: u64,
    abandoned: u64,
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

    pub(super) fn try_send(
        self: &Arc<Self>,
        sender: &SyncSender<QueuedCommand>,
        command: WriterCommand,
    ) -> Result<(), TrySendError<QueuedCommand>> {
        let queued = QueuedCommand::new(command, Arc::clone(self));
        let accepted = Arc::clone(&queued.ticket.accepted);
        // Only a nonblocking send occurs under this lock. A receiver/drop waits
        // until successful admission is counted; failed envelopes leave the lock
        // before their destructors run. No telemetry callback or SQL executes here.
        let mut state = self.queue.lock().unwrap_or_else(|p| p.into_inner());
        let result = sender.try_send(queued);
        if result.is_ok() {
            accepted.store(true, Ordering::Relaxed);
            state.queued = state.queued.saturating_add(1);
            state.peak = state.peak.max(state.queued);
        }
        result
    }

    pub(super) fn snapshot(&self) -> GlobalDispatchMetricsSnapshot {
        let queue = self.queue.lock().unwrap_or_else(|p| p.into_inner());
        GlobalDispatchMetricsSnapshot {
            queued: queue.queued,
            peak_queued: queue.peak,
            abandoned: queue.abandoned,
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
    ticket: QueueTicket,
}

// Exactly one ticket follows each envelope. Receiver destruction/unwind needs no
// special bulk-reset hook and cannot accidentally clear another live envelope.
struct QueueTicket {
    metrics: Option<Arc<DispatchMetrics>>,
    accepted: Arc<AtomicBool>,
}

impl QueueTicket {
    fn finish(&mut self, abandoned: bool) {
        let Some(metrics) = self.metrics.take() else {
            return;
        };
        let mut state = metrics.queue.lock().unwrap_or_else(|p| p.into_inner());
        if self.accepted.load(Ordering::Relaxed) {
            state.queued = state.queued.saturating_sub(1);
            if abandoned {
                state.abandoned = state.abandoned.saturating_add(1);
            }
        }
    }
}

impl Drop for QueueTicket {
    fn drop(&mut self) {
        self.finish(true);
    }
}

impl QueuedCommand {
    pub(super) fn new(command: WriterCommand, metrics: Arc<DispatchMetrics>) -> Self {
        Self {
            command,
            ticket: QueueTicket {
                metrics: Some(Arc::clone(&metrics)),
                accepted: Arc::new(AtomicBool::new(false)),
            },
            metrics,
            submitted: Instant::now(),
        }
    }

    pub(super) fn received(mut self) -> (WriterCommand, PendingDispatch) {
        let wait = self.submitted.elapsed();
        self.ticket.finish(false);
        (
            self.command,
            PendingDispatch {
                metrics: self.metrics,
                wait,
            },
        )
    }

    /// Profile-loss draining/discard does not become normal dispatch evidence.
    pub(super) fn discard(mut self) -> WriterCommand {
        self.ticket.finish(true);
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
