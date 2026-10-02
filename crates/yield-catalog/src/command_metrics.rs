//! Bounded observations at catalog submission and actual worker receipt.

use std::time::{Duration, Instant};

use super::CatalogSubmitError;

const PENDING_CAPACITY: usize = super::COMMAND_CAPACITY + 1;

/// Fixed-size monotonic duration observations in microseconds.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CatalogDurationMetrics {
    /// Number of observed boundary crossings, saturating at `u64::MAX`.
    pub count: u64,
    /// Sum of observed microseconds, saturating at `u64::MAX`.
    pub total_micros: u64,
    /// Largest observed duration in microseconds.
    pub max_micros: u64,
    /// Disjoint buckets: under 100us, under 1ms, under 10ms, under 100ms, and the remainder.
    pub buckets: [u64; 5],
}

impl CatalogDurationMetrics {
    fn record(&mut self, duration: Duration) {
        let micros = u64::try_from(duration.as_micros()).unwrap_or(u64::MAX);
        self.count = self.count.saturating_add(1);
        self.total_micros = self.total_micros.saturating_add(micros);
        self.max_micros = self.max_micros.max(micros);
        let bucket = match micros {
            0..100 => 0,
            100..1_000 => 1,
            1_000..10_000 => 2,
            10_000..100_000 => 3,
            _ => 4,
        };
        self.buckets[bucket] = self.buckets[bucket].saturating_add(1);
    }
}

/// Acquisition-local catalog admission and pending-dispatch observations.
///
/// Admission and dispatch do not imply an owned effect, durable commit or successful result.
/// No identifiers, paths or command payloads are retained. Counters saturate rather than wrap.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CatalogCommandMetrics {
    /// Commands accepted into the existing bounded channel.
    pub accepted: u64,
    /// Commands rejected by channel, event-credit or payload-credit limits.
    pub full: u64,
    /// Commands rejected by shutdown, closed admission or a disconnected worker.
    pub closed: u64,
    /// Commands rejected by existing input or curation validation.
    pub invalid: u64,
    /// Commands admitted but not yet observed at the worker receipt boundary.
    ///
    /// Valid only while `dispatch_timing_available` is true. Includes one command received while
    /// waiting for the admission mutex; this is not exact
    /// channel occupancy. The bound is channel capacity plus one, currently 33.
    pub pending_dispatch: u32,
    /// Largest observed pending-dispatch count in this owner lifetime.
    pub pending_high_water: u32,
    /// Worker receipt boundaries with an actual accepted submission timestamp.
    pub dispatched: u64,
    /// Receipt boundaries without a tracked timestamp, including direct test-channel injections.
    pub untracked_dispatches: u64,
    /// Whether pending depth and dispatch duration retain complete timestamp correlation.
    /// False after an unexpected bookkeeping loss; receipt counters then remain untracked.
    pub dispatch_timing_available: bool,
    /// Unexpected timestamp-capacity loss; zero for the normal serialized production route.
    pub bookkeeping_overflow: u64,
    /// Time spent acquiring the existing admission mutex, distinct from SQLite writer wait.
    pub admission_gate_wait: CatalogDurationMetrics,
    /// Time from accepted channel send attempt to worker receipt, including admission-lock wait.
    pub dispatch_wait: CatalogDurationMetrics,
}

pub(super) struct CommandMetrics {
    snapshot: CatalogCommandMetrics,
    pending: [Option<Instant>; PENDING_CAPACITY],
    head: usize,
    len: usize,
}

impl CommandMetrics {
    pub(super) fn new() -> Self {
        Self {
            snapshot: CatalogCommandMetrics {
                dispatch_timing_available: true,
                ..CatalogCommandMetrics::default()
            },
            pending: [None; PENDING_CAPACITY],
            head: 0,
            len: 0,
        }
    }

    pub(super) fn snapshot(&self) -> CatalogCommandMetrics {
        self.snapshot
    }

    pub(super) fn submission(
        &mut self,
        result: &Result<(), CatalogSubmitError>,
        gate_wait: Duration,
    ) {
        self.snapshot.admission_gate_wait.record(gate_wait);
        let counter = match result {
            Ok(()) => &mut self.snapshot.accepted,
            Err(CatalogSubmitError::Full) => &mut self.snapshot.full,
            Err(CatalogSubmitError::Closed) => &mut self.snapshot.closed,
            Err(_) => &mut self.snapshot.invalid,
        };
        *counter = counter.saturating_add(1);
    }

    pub(super) fn accepted(&mut self, sent_at: Instant) {
        if !self.snapshot.dispatch_timing_available {
            return;
        }
        if self.len == PENDING_CAPACITY {
            // Preserve an explicit unknown observation instead of mis-correlating later receipts.
            self.pending.fill(None);
            self.len = 0;
            self.snapshot.pending_dispatch = 0;
            self.snapshot.dispatch_timing_available = false;
            self.snapshot.bookkeeping_overflow =
                self.snapshot.bookkeeping_overflow.saturating_add(1);
            return;
        }
        self.pending[(self.head + self.len) % PENDING_CAPACITY] = Some(sent_at);
        self.len += 1;
        self.snapshot.pending_dispatch = self.len as u32;
        self.snapshot.pending_high_water = self.snapshot.pending_high_water.max(self.len as u32);
    }

    pub(super) fn received(&mut self, now: Instant) {
        if self.len == 0 {
            self.snapshot.untracked_dispatches =
                self.snapshot.untracked_dispatches.saturating_add(1);
            return;
        }
        let sent_at = self.pending[self.head]
            .take()
            .expect("occupied timestamp slot");
        self.head = (self.head + 1) % PENDING_CAPACITY;
        self.len -= 1;
        self.snapshot.pending_dispatch = self.len as u32;
        self.snapshot.dispatched = self.snapshot.dispatched.saturating_add(1);
        self.snapshot
            .dispatch_wait
            .record(now.saturating_duration_since(sent_at));
    }
}

#[cfg(test)]
mod tests;
