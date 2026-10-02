//! Fixed aggregate observations for the existing snapshot worker channel.
use super::{SnapshotCommand, mpsc};
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Instant;

/// Acquisition-local channel observations, never source authority or durable completion.
/// Fields are sampled independently and can temporarily disagree during concurrent admission.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SourceSnapshotQueueMetrics {
    /// Actual page/tag request enqueue attempts; fixture control commands are excluded.
    pub attempts: u64,
    /// Attempts accepted by the bounded channel, not successful reads or effects.
    pub accepted: u64,
    /// Attempts rejected because the channel buffer was full.
    pub full: u64,
    /// Attempts rejected because the worker receiver was disconnected.
    pub disconnected: u64,
    /// Pending dispatch reservations, including an enqueue attempt before its result is known.
    /// Failed admission, dequeue and dropped envelopes refund exactly one reservation.
    /// This is not an exact physical channel-buffer depth.
    pub pending_dispatch: u64,
    /// Highest observed reservation count, including concurrent offered admissions.
    pub peak_pending_dispatch: u64,
    /// Real page/tag requests received by the worker, including already-expired requests.
    pub dequeued: u64,
    /// Sum of monotonic microseconds from immediately before try_send to worker receipt.
    /// Request construction and SQL execution are outside this boundary.
    pub dispatch_wait_micros: u64,
    /// Largest observed dispatch wait, with the same boundary as the sum.
    pub max_dispatch_wait_micros: u64,
    /// Non-cumulative dispatch counts in <=100us, <=1ms, <=5ms, <=20ms, <=100ms,
    /// <=500ms, <=2s, and >2s buckets. All counters saturate.
    pub dispatch_wait_buckets: [u64; 8],
}

#[derive(Default)]
pub(super) struct SnapshotCounters {
    attempts: AtomicU64,
    accepted: AtomicU64,
    full: AtomicU64,
    disconnected: AtomicU64,
    pending: AtomicU64,
    peak: AtomicU64,
    dequeued: AtomicU64,
    wait: AtomicU64,
    max_wait: AtomicU64,
    buckets: [AtomicU64; 8],
}

fn add(counter: &AtomicU64, amount: u64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        Some(value.saturating_add(amount))
    });
}

impl SnapshotCounters {
    pub(super) fn snapshot(&self) -> SourceSnapshotQueueMetrics {
        let read = |value: &AtomicU64| value.load(Ordering::Relaxed);
        SourceSnapshotQueueMetrics {
            attempts: read(&self.attempts),
            accepted: read(&self.accepted),
            full: read(&self.full),
            disconnected: read(&self.disconnected),
            pending_dispatch: read(&self.pending),
            peak_pending_dispatch: read(&self.peak),
            dequeued: read(&self.dequeued),
            dispatch_wait_micros: read(&self.wait),
            max_dispatch_wait_micros: read(&self.max_wait),
            dispatch_wait_buckets: std::array::from_fn(|index| read(&self.buckets[index])),
        }
    }
}

struct DispatchObservation {
    counters: Arc<SnapshotCounters>,
    started: Instant,
    reserved: bool,
}

impl DispatchObservation {
    fn new(counters: Arc<SnapshotCounters>) -> Self {
        add(&counters.attempts, 1);
        let reserved = counters
            .pending
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .is_ok();
        counters
            .peak
            .fetch_max(counters.pending.load(Ordering::Relaxed), Ordering::Relaxed);
        Self {
            counters,
            started: Instant::now(),
            reserved,
        }
    }

    fn dispatch(&self) {
        let micros = u64::try_from(self.started.elapsed().as_micros()).unwrap_or(u64::MAX);
        add(&self.counters.dequeued, 1);
        add(&self.counters.wait, micros);
        self.counters.max_wait.fetch_max(micros, Ordering::Relaxed);
        let bucket = [100, 1_000, 5_000, 20_000, 100_000, 500_000, 2_000_000]
            .iter()
            .position(|bound| micros <= *bound)
            .unwrap_or(7);
        add(&self.counters.buckets[bucket], 1);
    }
}

impl Drop for DispatchObservation {
    fn drop(&mut self) {
        if self.reserved {
            let _ =
                self.counters
                    .pending
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                        Some(value.saturating_sub(1))
                    });
        }
    }
}

pub(super) struct SnapshotEnvelope {
    command: SnapshotCommand,
    observation: Option<DispatchObservation>,
}

impl SnapshotEnvelope {
    pub(super) fn dispatch(self) -> SnapshotCommand {
        if let Some(observation) = &self.observation {
            observation.dispatch();
        }
        self.command
    }
}

pub(super) struct SnapshotSender {
    sender: mpsc::SyncSender<SnapshotEnvelope>,
    counters: Arc<SnapshotCounters>,
}

impl SnapshotSender {
    pub(super) fn channel() -> (
        Self,
        mpsc::Receiver<SnapshotEnvelope>,
        Arc<SnapshotCounters>,
    ) {
        // Preserve the existing one-slot capacity and sole worker transport.
        let (sender, receiver) = mpsc::sync_channel(1);
        let counters = Arc::new(SnapshotCounters::default());
        (
            Self {
                sender,
                counters: counters.clone(),
            },
            receiver,
            counters,
        )
    }

    pub(super) fn try_send(
        &self,
        command: SnapshotCommand,
    ) -> Result<(), mpsc::TrySendError<SnapshotCommand>> {
        let tracked = matches!(
            &command,
            SnapshotCommand::Page(_) | SnapshotCommand::TagSelection(_)
        );
        let envelope = SnapshotEnvelope {
            command,
            observation: tracked.then(|| DispatchObservation::new(self.counters.clone())),
        };
        match self.sender.try_send(envelope) {
            Ok(()) => {
                if tracked {
                    add(&self.counters.accepted, 1);
                }
                Ok(())
            }
            Err(mpsc::TrySendError::Full(envelope)) => {
                if tracked {
                    add(&self.counters.full, 1);
                }
                Err(mpsc::TrySendError::Full(envelope.command))
            }
            Err(mpsc::TrySendError::Disconnected(envelope)) => {
                if tracked {
                    add(&self.counters.disconnected, 1);
                }
                Err(mpsc::TrySendError::Disconnected(envelope.command))
            }
        }
    }

    #[cfg(test)]
    pub(super) fn send(
        &self,
        command: SnapshotCommand,
    ) -> Result<(), mpsc::SendError<SnapshotCommand>> {
        assert!(!matches!(
            &command,
            SnapshotCommand::Page(_) | SnapshotCommand::TagSelection(_)
        ));
        self.sender
            .send(SnapshotEnvelope {
                command,
                observation: None,
            })
            .map_err(|error| mpsc::SendError(error.0.command))
    }
}

#[cfg(test)]
mod tests {
    use super::super::{RootIdentity, SnapshotRequest, SourceId, SourceRevision};
    use super::*;
    use std::time::Duration;

    fn request() -> SnapshotCommand {
        let (reply, _) = mpsc::channel();
        SnapshotCommand::Page(SnapshotRequest {
            expected: SourceRevision {
                source_id: SourceId::from_string("queue-observation-fixture"),
                root_identity: RootIdentity::from_bytes(vec![1]),
                root_binding_identity: "fixture".into(),
                database_identity: "fixture".into(),
                lifecycle_generation: 1,
                lease_epoch: 1,
                revision: 1,
            },
            after_cursor: None,
            requested_limit: 1,
            deadline: Instant::now() + Duration::from_secs(1),
            reply,
        })
    }

    #[test]
    fn rejected_admissions_refund_and_do_not_record_dispatch() {
        let (sender, receiver, counters) = SnapshotSender::channel();
        assert!(sender.try_send(request()).is_ok());
        assert!(matches!(
            sender.try_send(request()),
            Err(mpsc::TrySendError::Full(_))
        ));
        let metrics = counters.snapshot();
        assert_eq!(
            (
                metrics.attempts,
                metrics.accepted,
                metrics.full,
                metrics.pending_dispatch
            ),
            (2, 1, 1, 1)
        );
        assert_eq!(metrics.dequeued, 0);
        let _ = receiver.recv().unwrap().dispatch();
        assert_eq!(counters.snapshot().pending_dispatch, 0);
        assert_eq!(
            counters
                .snapshot()
                .dispatch_wait_buckets
                .iter()
                .sum::<u64>(),
            1
        );
        drop(receiver);
        assert!(matches!(
            sender.try_send(request()),
            Err(mpsc::TrySendError::Disconnected(_))
        ));
        assert_eq!(counters.snapshot().disconnected, 1);
        assert_eq!(counters.snapshot().pending_dispatch, 0);
        assert_eq!(counters.snapshot().dequeued, 1);
    }

    #[test]
    fn observations_saturate_without_labels_or_history() {
        let (sender, receiver, counters) = SnapshotSender::channel();
        counters.attempts.store(u64::MAX, Ordering::Relaxed);
        counters.accepted.store(u64::MAX, Ordering::Relaxed);
        counters.dequeued.store(u64::MAX, Ordering::Relaxed);
        counters.wait.store(u64::MAX, Ordering::Relaxed);
        for bucket in &counters.buckets {
            bucket.store(u64::MAX, Ordering::Relaxed);
        }
        assert!(sender.try_send(request()).is_ok());
        let _ = receiver.recv().unwrap().dispatch();
        let metrics = counters.snapshot();
        assert_eq!(metrics.attempts, u64::MAX);
        assert_eq!(metrics.accepted, u64::MAX);
        assert_eq!(metrics.dequeued, u64::MAX);
        assert_eq!(metrics.dispatch_wait_micros, u64::MAX);
        assert_eq!(metrics.dispatch_wait_buckets, [u64::MAX; 8]);
        assert_eq!(metrics.pending_dispatch, 0);
    }

    #[test]
    fn dropped_envelope_and_controls_keep_reservations_honest() {
        let (sender, receiver, counters) = SnapshotSender::channel();
        assert!(sender.try_send(request()).is_ok());
        drop(receiver);
        assert_eq!(counters.snapshot().pending_dispatch, 0);
        assert_eq!(counters.snapshot().dequeued, 0);
        let (sender, receiver, counters) = SnapshotSender::channel();
        let (reply, _) = mpsc::channel();
        assert!(sender.send(SnapshotCommand::BarrierForTest(reply)).is_ok());
        let _ = receiver.recv().unwrap().dispatch();
        assert_eq!(counters.snapshot(), SourceSnapshotQueueMetrics::default());
    }
}
