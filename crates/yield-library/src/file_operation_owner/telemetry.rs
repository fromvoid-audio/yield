//! Fixed aggregate observations at the effect submission and dequeue boundaries.
use std::sync::Mutex;
use std::time::Duration;

use super::{EffectOutcome, FileOperationOwnerQueueError};

/// Fixed admission categories; successful submission does not imply an effect succeeded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
pub enum FileOwnerAdmissionOutcome {
    /// An effect request entered the owner queue, including exact duplicate requests.
    Accepted,
    /// The command channel was full.
    QueueFull,
    /// The result-handle or retained-receipt limit was reached.
    ResultsFull,
    /// The retained input-byte budget was exhausted.
    InputBytesFull,
    /// The canonical request or backing-byte bound was exceeded.
    RecordTooLarge,
    /// Input validation failed before enqueue.
    InvalidInput,
    /// The owner was unavailable or its channel disconnected.
    Unavailable,
    /// The owner was draining.
    Draining,
    /// The owner was stopped.
    Stopped,
}

/// Path-free, fixed-size observations for one owner lifetime.
///
/// This snapshot is internally consistent under a short telemetry lock, but submit counters
/// are recorded after enqueue: a concurrent dequeue may precede its Accepted counter update.
/// No observation grants authority, certifies an effect, or describes durable custody.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FileOwnerTelemetrySnapshot {
    /// Effect envelopes waiting to be dequeued; excludes running effects and control commands.
    pub queued_effects: u64,
    /// Largest observed successfully enqueued effect backlog; rejected sends do not contribute.
    pub peak_queued_effects: u64,
    /// Enqueued effects discarded when the worker exited without dequeuing them.
    pub abandoned_effects: u64,
    /// Saturating submission counters indexed by [`FileOwnerAdmissionOutcome`].
    pub admissions: [u64; 9],
    /// Dispatch-wait counts in exclusive buckets: <=1ms, <=10ms, <=100ms, <=1s, <=10s, >10s.
    ///
    /// Timing begins immediately before envelope construction/send and ends at dequeue,
    /// before validation/execution. It excludes canonicalization, admission-gate wait,
    /// SQL, physical effects, control commands and receipt retention after execution.
    pub dispatch_wait_buckets: [u64; 6],
}

/// Fixed original retained primary-result categories. No category grants effect authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
pub enum FileOwnerRetainedOutcome {
    /// Applied observation or effect; readonly validation is not publication.
    Applied,
    /// Original live fence refused before effect.
    SupersededBeforeEffect,
    /// Original identity or absence predicate collided.
    Collision,
    /// No qualified producer/backend exists.
    Unsupported,
    /// Original typed before-effect retry outcome, not retry authorization.
    RetryableBeforeEffect,
    /// Original outcome retains uncertainty.
    RetainedUncertain,
    /// Original result reports its existing safe barrier.
    StoppedAtSafeBoundary,
    /// Original changed-request conflict observation.
    Conflict,
}
impl FileOwnerRetainedOutcome {
    pub(super) fn from_outcome(outcome: &EffectOutcome) -> Self {
        match outcome {
            EffectOutcome::Applied { .. } => Self::Applied,
            EffectOutcome::SupersededBeforeEffect { .. } => Self::SupersededBeforeEffect,
            EffectOutcome::Collision { .. } => Self::Collision,
            EffectOutcome::Unsupported { .. } => Self::Unsupported,
            EffectOutcome::RetryableBeforeEffect { .. } => Self::RetryableBeforeEffect,
            EffectOutcome::RetainedUncertain { .. } => Self::RetainedUncertain,
            EffectOutcome::StoppedAtSafeBoundary { .. } => Self::StoppedAtSafeBoundary,
            EffectOutcome::Conflict { .. } => Self::Conflict,
        }
    }
}
/// Fixed observations of original worker-held result inventories.
/// This is not durable custody, physical qualification or a settlement acknowledgement.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FileOwnerRetainedTelemetrySnapshot {
    /// Current worker receipt entries, including replies lost before acknowledgement.
    pub receipts: u64,
    /// Current acknowledgement tombstones; acknowledgement does not settle an effect.
    pub tombstones: u64,
    /// Saturating counts indexed by FileOwnerRetainedOutcome, once per newly retained primary.
    pub primary_outcomes: [u64; 8],
}

#[derive(Default)]
pub(super) struct FileOwnerTelemetry(
    Mutex<FileOwnerTelemetrySnapshot>,
    Mutex<FileOwnerRetainedTelemetrySnapshot>,
);

impl FileOwnerTelemetry {
    pub(super) fn retained_snapshot(&self) -> Option<FileOwnerRetainedTelemetrySnapshot> {
        match self.1.try_lock() {
            Ok(state) => Some(*state),
            Err(std::sync::TryLockError::WouldBlock) => None,
            Err(std::sync::TryLockError::Poisoned(poison)) => Some(*poison.into_inner()),
        }
    }
    pub(super) fn retain_primary(
        &self,
        category: FileOwnerRetainedOutcome,
        receipts: usize,
        tombstones: usize,
    ) {
        let mut state = self.1.lock().unwrap_or_else(|poison| poison.into_inner());
        let count = &mut state.primary_outcomes[category as usize];
        *count = count.saturating_add(1);
        state.receipts = receipts as u64;
        state.tombstones = tombstones as u64;
    }
    pub(super) fn retained_inventory(&self, receipts: usize, tombstones: usize) {
        let mut state = self.1.lock().unwrap_or_else(|poison| poison.into_inner());
        state.receipts = receipts as u64;
        state.tombstones = tombstones as u64;
    }

    pub(super) fn snapshot(&self) -> FileOwnerTelemetrySnapshot {
        self.0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    pub(super) fn record_admission(&self, error: Option<&FileOperationOwnerQueueError>) {
        let outcome = match error {
            None => FileOwnerAdmissionOutcome::Accepted,
            Some(FileOperationOwnerQueueError::Full) => FileOwnerAdmissionOutcome::QueueFull,
            Some(FileOperationOwnerQueueError::ResultsFull) => {
                FileOwnerAdmissionOutcome::ResultsFull
            }
            Some(FileOperationOwnerQueueError::InputBytesFull { .. }) => {
                FileOwnerAdmissionOutcome::InputBytesFull
            }
            Some(FileOperationOwnerQueueError::RecordTooLarge { .. }) => {
                FileOwnerAdmissionOutcome::RecordTooLarge
            }
            Some(FileOperationOwnerQueueError::InvalidInput(_)) => {
                FileOwnerAdmissionOutcome::InvalidInput
            }
            Some(FileOperationOwnerQueueError::Unavailable(_)) => {
                FileOwnerAdmissionOutcome::Unavailable
            }
            Some(FileOperationOwnerQueueError::Draining) => FileOwnerAdmissionOutcome::Draining,
            Some(FileOperationOwnerQueueError::Stopped) => FileOwnerAdmissionOutcome::Stopped,
        };
        let mut state = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        let count = &mut state.admissions[outcome as usize];
        *count = count.saturating_add(1);
    }

    // Serialize the nonblocking send and backlog increment with dequeue observation.
    // Never drop a failed envelope under this lock; return it to the existing refund path.
    pub(super) fn enqueue<T, E>(&self, send: impl FnOnce() -> Result<T, E>) -> Result<T, E> {
        let mut state = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        let result = send();
        if result.is_ok() {
            state.queued_effects = state.queued_effects.saturating_add(1);
            state.peak_queued_effects = state.peak_queued_effects.max(state.queued_effects);
        }
        result
    }

    pub(super) fn dispatch(&self, elapsed: Duration) {
        let mut state = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        state.queued_effects = state.queued_effects.saturating_sub(1);
        let index = [1, 10, 100, 1000, 10000]
            .iter()
            .position(|millis| elapsed <= Duration::from_millis(*millis))
            .unwrap_or(5);
        state.dispatch_wait_buckets[index] = state.dispatch_wait_buckets[index].saturating_add(1);
    }

    pub(super) fn abandon_waiting(&self) {
        let mut state = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        state.abandoned_effects = state.abandoned_effects.saturating_add(state.queued_effects);
        state.queued_effects = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_numeric_snapshot_is_fixed_saturating_and_nonblocking() {
        let metrics = FileOwnerTelemetry::default();
        assert_eq!(
            std::mem::size_of::<FileOwnerRetainedTelemetrySnapshot>(),
            80
        );
        assert!(std::mem::size_of::<FileOwnerTelemetry>() <= 256);
        {
            let mut state = metrics.1.lock().unwrap();
            state.primary_outcomes[FileOwnerRetainedOutcome::Unsupported as usize] = u64::MAX;
            assert_eq!(metrics.retained_snapshot(), None);
        }
        metrics.retain_primary(FileOwnerRetainedOutcome::Unsupported, 3, 4);
        let snapshot = metrics.retained_snapshot().unwrap();
        assert_eq!((snapshot.receipts, snapshot.tombstones), (3, 4));
        assert_eq!(
            snapshot.primary_outcomes[FileOwnerRetainedOutcome::Unsupported as usize],
            u64::MAX
        );
        metrics.retained_inventory(0, 0);
        assert_eq!(
            metrics.retained_snapshot().unwrap().primary_outcomes,
            snapshot.primary_outcomes
        );
    }

    #[test]
    fn fixed_dispatch_boundaries_and_saturating_counts() {
        let metrics = FileOwnerTelemetry::default();
        for millis in [0, 1, 2, 10, 11, 100, 101, 1000, 1001, 10000, 10001] {
            metrics.enqueue(|| Ok::<_, ()>(())).unwrap();
            metrics.dispatch(Duration::from_millis(millis));
        }
        assert_eq!(metrics.snapshot().dispatch_wait_buckets, [2, 2, 2, 2, 2, 1]);
        {
            let mut state = metrics.0.lock().unwrap();
            state.admissions[0] = u64::MAX;
            state.dispatch_wait_buckets[0] = u64::MAX;
            state.abandoned_effects = u64::MAX;
            state.queued_effects = 1;
        }
        metrics.record_admission(None);
        metrics.dispatch(Duration::ZERO);
        metrics.enqueue(|| Ok::<_, ()>(())).unwrap();
        metrics.abandon_waiting();
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.admissions[0], u64::MAX);
        assert_eq!(snapshot.dispatch_wait_buckets[0], u64::MAX);
        assert_eq!(snapshot.abandoned_effects, u64::MAX);
        assert_eq!(snapshot.queued_effects, 0);
    }

    #[test]
    fn all_rejection_categories_are_fixed_and_payload_free() {
        let metrics = FileOwnerTelemetry::default();
        let errors = [
            FileOperationOwnerQueueError::Full,
            FileOperationOwnerQueueError::ResultsFull,
            FileOperationOwnerQueueError::InputBytesFull { requested: 123 },
            FileOperationOwnerQueueError::RecordTooLarge { actual: usize::MAX },
            FileOperationOwnerQueueError::InvalidInput(
                super::super::FileOperationValidationError::new("private locator"),
            ),
            FileOperationOwnerQueueError::Unavailable("private host error".into()),
            FileOperationOwnerQueueError::Draining,
            FileOperationOwnerQueueError::Stopped,
        ];
        for error in &errors {
            metrics.record_admission(Some(error));
        }
        assert_eq!(metrics.snapshot().admissions, [0, 1, 1, 1, 1, 1, 1, 1, 1]);
        assert!(!format!("{:?}", metrics.snapshot()).contains("private"));
        assert!(metrics.enqueue(|| Err::<(), _>(())).is_err());
        assert_eq!(metrics.snapshot().queued_effects, 0);
        assert_eq!(metrics.snapshot().peak_queued_effects, 0);
    }
}
