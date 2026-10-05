//! Original queue observation only; no storage, admission or settlement authority.
use super::{InputClass, JournalCommand, QueuedJournalCommand};
use std::sync::{
    Arc, Mutex, TryLockError,
    atomic::{AtomicBool, Ordering},
    mpsc::{SyncSender, TrySendError},
};
use std::time::Instant;

/// One original queue class's fixed saturating counters.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct JournalQueueClassMetrics {
    /// Successful nonblocking enqueues only.
    pub accepted: u64,
    /// Completed dequeue accounting, before command validation/execution.
    pub dequeued: u64,
    /// Accepted envelopes discarded before dispatch, including receiver teardown.
    pub abandoned: u64,
    /// Accepted minus accounted dequeue/discard. Includes the short accounting window.
    pub backlog: u64,
    /// Maximum backlog at that same boundary, potentially channel capacity plus one.
    pub peak_backlog: u64,
    /// Enqueue-attempt to accounted dequeue: <=1, <=10, <=100, <=1000, >1000 ms.
    pub dispatch_wait_buckets: [u64; 5],
}
/// Numeric observations from the original bounded ordinary and control channels.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct JournalQueueMetricsSnapshot {
    /// Original ordinary queue, including publication-funded commands.
    pub ordinary: JournalQueueClassMetrics,
    /// Original priority control queue.
    pub control: JournalQueueClassMetrics,
}
#[derive(Default)]
pub(super) struct QueueMetrics(Mutex<[JournalQueueClassMetrics; 2]>);
impl QueueMetrics {
    pub(super) fn snapshot(&self) -> Option<JournalQueueMetricsSnapshot> {
        let state = match self.0.try_lock() {
            Ok(state) => state,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            Err(TryLockError::WouldBlock) => return None,
        };
        Some(JournalQueueMetricsSnapshot {
            ordinary: state[0],
            control: state[1],
        })
    }
    pub(super) fn send(
        self: &Arc<Self>,
        sender: &SyncSender<QueuedJournalCommand>,
        class: InputClass,
        command: JournalCommand,
    ) -> Result<(), TrySendError<QueuedJournalCommand>> {
        let accepted = Arc::new(AtomicBool::new(false));
        let queued = QueuedJournalCommand {
            command,
            ticket: QueueTicket {
                metrics: Some(Arc::clone(self)),
                accepted: Arc::clone(&accepted),
                class,
                started: Instant::now(),
            },
        };
        let result;
        {
            let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
            // Only nonblocking send and numeric bookkeeping under this short mutex.
            // Keep rejected command/destructors in result until AFTER the mutex is released.
            result = sender.try_send(queued);
            if result.is_ok() {
                accepted.store(true, Ordering::Release);
                let counters = &mut state[class.index()];
                counters.accepted = counters.accepted.saturating_add(1);
                counters.backlog = counters.backlog.saturating_add(1);
                counters.peak_backlog = counters.peak_backlog.max(counters.backlog);
            }
        }
        result
    }
}
pub(super) struct QueueTicket {
    metrics: Option<Arc<QueueMetrics>>,
    accepted: Arc<AtomicBool>,
    class: InputClass,
    started: Instant,
}
impl QueueTicket {
    pub(super) fn dequeued(&mut self) {
        self.finish(true);
    }
    fn finish(&mut self, dispatched: bool) {
        let Some(metrics) = self.metrics.take() else {
            return;
        };
        let mut state = metrics.0.lock().unwrap_or_else(|error| error.into_inner());
        // Send may have handed this envelope to a receiver before completing bookkeeping.
        // This mutex joins that original completion boundary, including failed-send Drop.
        if !self.accepted.load(Ordering::Acquire) {
            return;
        }
        let counters = &mut state[self.class.index()];
        counters.backlog = counters.backlog.saturating_sub(1);
        if dispatched {
            counters.dequeued = counters.dequeued.saturating_add(1);
            let elapsed = self.started.elapsed();
            let bucket = [1, 10, 100, 1000]
                .iter()
                .position(|ms| elapsed <= std::time::Duration::from_millis(*ms))
                .unwrap_or(4);
            counters.dispatch_wait_buckets[bucket] =
                counters.dispatch_wait_buckets[bucket].saturating_add(1);
        } else {
            counters.abandoned = counters.abandoned.saturating_add(1);
        }
    }
}
impl Drop for QueueTicket {
    fn drop(&mut self) {
        self.finish(false);
    }
}

#[cfg(test)]
mod tests;
