//! Fixed observations of original durable retry returns, never executable retry authority.
use super::store::OperationJournalStoreError;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Original Store primitive being observed; nested calls keep their own identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
pub enum JournalRetryCall {
    /// Existing durable retry observation update or identical replay.
    RetryPending,
    /// Existing post-intent capacity exhaustion observation and status readback.
    ChunkExhaustion,
}

/// Borrowed original result category, not a new commit, disposition or permission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
pub enum JournalRetryReturn {
    /// Original success, including read-only idempotent replay.
    ConfirmedReturn,
    /// Original SQLite DatabaseBusy or DatabaseLocked error.
    ContentionError,
    /// Original named audit/retained/identity mismatch error; no diagnostic copied.
    RetainedAuditError,
    /// Any other original error; does not imply clean refusal or NoEffect.
    OtherError,
}

/// Independent process-lifetime samples, not a coherent snapshot or live gauge.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JournalRetryTelemetrySnapshot {
    counts: [[u64; 4]; 2],
    /// Original entry to return: <=1, <=10, <=100, <=1000, >1000 milliseconds.
    pub elapsed_buckets: [u64; 5],
}
impl JournalRetryTelemetrySnapshot {
    /// Saturating observations for the named original primitive and returned category.
    pub fn count(&self, call: JournalRetryCall, result: JournalRetryReturn) -> u64 {
        self.counts[call as usize][result as usize]
    }
}

struct Counters {
    counts: [[AtomicU64; 4]; 2],
    elapsed: [AtomicU64; 5],
}
impl Counters {
    const fn new() -> Self {
        Self {
            counts: [const { [const { AtomicU64::new(0) }; 4] }; 2],
            elapsed: [const { AtomicU64::new(0) }; 5],
        }
    }
    fn record(&self, call: JournalRetryCall, result: JournalRetryReturn, elapsed: Duration) {
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
    fn snapshot(&self) -> JournalRetryTelemetrySnapshot {
        JournalRetryTelemetrySnapshot {
            counts: std::array::from_fn(|i| {
                std::array::from_fn(|j| self.counts[i][j].load(Ordering::Relaxed))
            }),
            elapsed_buckets: std::array::from_fn(|i| self.elapsed[i].load(Ordering::Relaxed)),
        }
    }
}
static COUNTERS: Counters = Counters::new();

/// Read fixed numeric observations without owner, Store, identity or payload access.
pub fn journal_retry_telemetry_snapshot() -> JournalRetryTelemetrySnapshot {
    COUNTERS.snapshot()
}

fn category<T>(result: &Result<T, OperationJournalStoreError>) -> JournalRetryReturn {
    match result {
        Ok(_) => JournalRetryReturn::ConfirmedReturn,
        Err(OperationJournalStoreError::Sql(rusqlite::Error::SqliteFailure(error, _)))
            if matches!(
                error.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) =>
        {
            JournalRetryReturn::ContentionError
        }
        Err(
            OperationJournalStoreError::RequiresAudit { .. }
            | OperationJournalStoreError::RetainedRecord { .. }
            | OperationJournalStoreError::DatabaseReplaced { .. }
            | OperationJournalStoreError::ConnectionIdentityMismatch { .. },
        ) => JournalRetryReturn::RetainedAuditError,
        Err(_) => JournalRetryReturn::OtherError,
    }
}

pub(super) fn observe<T>(
    call: JournalRetryCall,
    invoke: impl FnOnce() -> Result<T, OperationJournalStoreError>,
) -> Result<T, OperationJournalStoreError> {
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
