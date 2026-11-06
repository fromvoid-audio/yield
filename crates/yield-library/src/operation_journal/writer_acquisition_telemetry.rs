//! Fixed observations of selected original exclusive transaction acquisitions.
use super::store::OperationJournalStoreError;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Original Store call site; these observations do not cover every journal writer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
pub enum JournalWriterCall {
    /// Original legacy-family intent insertion, including funded control calls.
    IntentInsert,
    /// Original durable cancellation observation update or exact replay.
    CancellationRequest,
    /// Original durable retry observation update or exact replay.
    RetryPending,
    /// Original ordinary-claim release receipt persistence or exact replay.
    OrdinaryClaimRelease,
    /// Original validated staging checkpoint persistence or exact replay.
    StagingCheckpoint,
    /// Original one-shot reservation of a validated stage for publication.
    PublicationReservation,
}

const CALL_COUNT: usize = JournalWriterCall::PublicationReservation as usize + 1;

/// Original acquisition return, before transaction body or commit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
pub enum JournalWriterReturn {
    /// Acquisition returned successfully; body and commit are not observed here.
    Acquired,
    /// Original SQLite DatabaseBusy, including extended BUSY codes.
    Busy,
    /// Original SQLite DatabaseLocked, including extended LOCKED codes.
    Locked,
    /// Any other original control/preparation failure; no diagnostic is copied.
    OtherError,
}

/// Process-lifetime saturating observations. Concurrent fields are approximate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JournalWriterTelemetrySnapshot {
    counts: [[u64; 4]; CALL_COUNT],
    /// Original acquisition call only: <=1, <=10, <=100, <=1000, >1000 ms.
    pub elapsed_buckets: [u64; 5],
}
impl JournalWriterTelemetrySnapshot {
    /// Count selected original acquisition returns, without granting authority.
    pub fn count(&self, call: JournalWriterCall, result: JournalWriterReturn) -> u64 {
        self.counts[call as usize][result as usize]
    }
}

struct Counters {
    counts: [[AtomicU64; 4]; CALL_COUNT],
    elapsed: [AtomicU64; 5],
}
impl Counters {
    const fn new() -> Self {
        Self {
            counts: [const { [const { AtomicU64::new(0) }; 4] }; CALL_COUNT],
            elapsed: [const { AtomicU64::new(0) }; 5],
        }
    }
    fn record(&self, call: JournalWriterCall, result: JournalWriterReturn, elapsed: Duration) {
        let increment = |counter: &AtomicU64| {
            let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                Some(value.saturating_add(1))
            });
        };
        increment(&self.counts[call as usize][result as usize]);
        let bucket = [1, 10, 100, 1000]
            .iter()
            .position(|ms| elapsed <= Duration::from_millis(*ms))
            .unwrap_or(4);
        increment(&self.elapsed[bucket]);
    }
    fn snapshot(&self) -> JournalWriterTelemetrySnapshot {
        JournalWriterTelemetrySnapshot {
            counts: std::array::from_fn(|i| {
                std::array::from_fn(|j| self.counts[i][j].load(Ordering::Relaxed))
            }),
            elapsed_buckets: std::array::from_fn(|i| self.elapsed[i].load(Ordering::Relaxed)),
        }
    }
}
static COUNTERS: Counters = Counters::new();

/// Numeric observations only; no retry, durability, capacity or effect authority.
pub fn journal_writer_telemetry_snapshot() -> JournalWriterTelemetrySnapshot {
    COUNTERS.snapshot()
}

fn category(result: &Result<(), OperationJournalStoreError>) -> JournalWriterReturn {
    match result {
        Ok(()) => JournalWriterReturn::Acquired,
        Err(OperationJournalStoreError::Sql(rusqlite::Error::SqliteFailure(error, _))) => {
            match error.code {
                rusqlite::ErrorCode::DatabaseBusy => JournalWriterReturn::Busy,
                rusqlite::ErrorCode::DatabaseLocked => JournalWriterReturn::Locked,
                _ => JournalWriterReturn::OtherError,
            }
        }
        Err(_) => JournalWriterReturn::OtherError,
    }
}

pub(super) fn observe(
    call: JournalWriterCall,
    invoke: impl FnOnce() -> Result<(), OperationJournalStoreError>,
) -> Result<(), OperationJournalStoreError> {
    let started = Instant::now();
    let result = invoke();
    let elapsed = started.elapsed();
    let returned = category(&result);
    COUNTERS.record(call, returned, elapsed);
    #[cfg(test)]
    tests::record(call, returned, elapsed);
    result
}

#[cfg(test)]
pub(super) mod tests;
