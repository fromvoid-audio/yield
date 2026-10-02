//! Lock-free recording health counters shared with the capture callback.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Observable health counters for one recording session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecordingHealth {
    /// Samples accepted into the writer ring, including any later abandoned samples.
    pub writer_accepted_samples: u64,
    /// Samples encoded in successful whole batches; this is not durable acknowledgement.
    pub writer_encoded_samples: u64,
    /// Accepted ring samples discarded without a codec attempt on cancellation or error.
    pub writer_abandoned_samples: u64,
    /// Samples in a failed codec batch; some may have been encoded before failure.
    pub writer_unconfirmed_samples: u64,
    /// The worker settled capture submissions and accounted for all accepted samples.
    /// This does not confirm finalization, native input shutdown or publication.
    pub writer_accounting_complete: bool,
    /// A writer counter saturated; exact sample/loss totals are unavailable.
    pub writer_accounting_overflowed: bool,
    /// Samples dropped because the bounded writer ring was full, stopped or failed.
    pub writer_dropped_samples: u64,
    /// Capture callbacks that encountered a full WAV-writer ring.
    pub writer_overrun_events: u64,
    /// Samples dropped because the bounded monitor ring was full.
    pub monitor_dropped_samples: u64,
    /// Capture callbacks that encountered a full monitor ring.
    pub monitor_overrun_events: u64,
    /// Monitor attach/detach commands rejected by the bounded control queue.
    pub monitor_control_drops: u64,
    /// Whether the WAV writer exited with an error.
    pub writer_failed: bool,
    /// Whether the monitor worker exited unexpectedly.
    pub monitor_failed: bool,
    /// Whether the monitor control worker disconnected.
    pub monitor_disconnected: bool,
}

#[derive(Default)]
pub(super) struct RecordingHealthState {
    pub(super) writer_accepted_samples: AtomicU64,
    pub(super) writer_encoded_samples: AtomicU64,
    pub(super) writer_abandoned_samples: AtomicU64,
    pub(super) writer_unconfirmed_samples: AtomicU64,
    pub(super) writer_accounting_complete: AtomicBool,
    pub(super) writer_accounting_overflowed: AtomicBool,
    // RecorderCapture has one exclusively borrowed producer. The worker waits; capture never does.
    pub(super) capture_in_flight: AtomicBool,
    pub(super) writer_dropped_samples: AtomicU64,
    pub(super) writer_overrun_events: AtomicU64,
    pub(super) monitor_dropped_samples: AtomicU64,
    pub(super) monitor_overrun_events: AtomicU64,
    pub(super) monitor_control_drops: AtomicU64,
    pub(super) writer_failed: AtomicBool,
    pub(super) monitor_failed: AtomicBool,
    pub(super) monitor_disconnected: AtomicBool,
}

impl RecordingHealthState {
    pub(super) fn add_writer_count(&self, counter: &AtomicU64, count: u64) {
        let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
            Some(old.checked_add(count).unwrap_or_else(|| {
                self.writer_accounting_overflowed
                    .store(true, Ordering::Release);
                u64::MAX
            }))
        });
    }

    pub(super) fn snapshot(&self) -> RecordingHealth {
        // Read the release marker before the counters. Live snapshots are observations, not an
        // atomic partition; after join, accepted/encoded/abandoned/unconfirmed are stable.
        let complete = self.writer_accounting_complete.load(Ordering::Acquire);
        RecordingHealth {
            writer_accepted_samples: self.writer_accepted_samples.load(Ordering::Relaxed),
            writer_encoded_samples: self.writer_encoded_samples.load(Ordering::Relaxed),
            writer_abandoned_samples: self.writer_abandoned_samples.load(Ordering::Relaxed),
            writer_unconfirmed_samples: self.writer_unconfirmed_samples.load(Ordering::Relaxed),
            writer_accounting_complete: complete,
            writer_accounting_overflowed: self.writer_accounting_overflowed.load(Ordering::Acquire),
            writer_dropped_samples: self.writer_dropped_samples.load(Ordering::Relaxed),
            writer_overrun_events: self.writer_overrun_events.load(Ordering::Relaxed),
            monitor_dropped_samples: self.monitor_dropped_samples.load(Ordering::Relaxed),
            monitor_overrun_events: self.monitor_overrun_events.load(Ordering::Relaxed),
            monitor_control_drops: self.monitor_control_drops.load(Ordering::Relaxed),
            writer_failed: self.writer_failed.load(Ordering::Acquire),
            monitor_failed: self.monitor_failed.load(Ordering::Acquire),
            monitor_disconnected: self.monitor_disconnected.load(Ordering::Acquire),
        }
    }
}
