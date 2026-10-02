//! Ordinary global metadata acquisition only. Never retry a body or a commit.

use std::time::{Duration, Instant};

mod metrics;
pub use metrics::{GlobalAcquisitionMetricsSnapshot, global_acquisition_metrics_snapshot};

use rusqlite::{Connection, Transaction, TransactionBehavior, ffi};

use super::{LibraryError, error::map_sql_error};

const MAX_ATTEMPTS: u8 = 3;

pub(super) fn begin(connection: &Connection) -> Result<Transaction<'_>, LibraryError> {
    begin_with_backoff(connection, |attempt| {
        std::thread::sleep(Duration::from_millis(u64::from(attempt) * 5));
    })
}

fn begin_with_backoff(
    connection: &Connection,
    backoff: impl FnMut(u8),
) -> Result<Transaction<'_>, LibraryError> {
    begin_observed(connection, backoff, &metrics::PROCESS_METRICS)
}

fn begin_observed<'a>(
    connection: &'a Connection,
    backoff: impl FnMut(u8),
    metrics: &metrics::AcquisitionMetrics,
) -> Result<Transaction<'a>, LibraryError> {
    let started = Instant::now();
    let nested = !connection.is_autocommit();
    let result = begin_inner(connection, backoff, metrics);
    metrics.finish(&result, nested, started.elapsed());
    result
}

fn begin_inner<'a>(
    connection: &'a Connection,
    mut backoff: impl FnMut(u8),
    metrics: &metrics::AcquisitionMetrics,
) -> Result<Transaction<'a>, LibraryError> {
    // Do not disturb a caller's active transaction or retry an ambiguous acquisition.
    if !connection.is_autocommit() {
        return Err(map_sql_error(rusqlite::Error::InvalidQuery));
    }
    let timeout: u64 = connection
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .map_err(map_sql_error)?;
    connection
        .busy_timeout(Duration::ZERO)
        .map_err(map_sql_error)?;
    let result = (|| {
        for attempt in 1..=MAX_ATTEMPTS {
            metrics.attempt();
            match Transaction::new_unchecked(connection, TransactionBehavior::Immediate) {
                Ok(transaction) => return Ok(transaction),
                Err(error) => {
                    let Some(code) = contention_code(&error) else {
                        return Err(map_sql_error(error));
                    };
                    // Only plain BUSY before a transaction began is a retryable reservation.
                    // LOCKED and extended BUSY (including SNAPSHOT) retain their exact code.
                    let retry = code == ffi::SQLITE_BUSY
                        && connection.is_autocommit()
                        && attempt < MAX_ATTEMPTS;
                    tracing::info!(
                        target: "perf::global_db",
                        action = "ordinary_writer_acquisition",
                        attempt,
                        sqlite_code = code,
                        retry,
                        "Ordinary metadata writer contention before effect"
                    );
                    if !retry {
                        return Err(LibraryError::OrdinaryWriteContention {
                            attempts: attempt,
                            sqlite_code: code,
                        });
                    }
                    metrics.retry();
                    backoff(attempt);
                }
            }
        }
        unreachable!("finite attempts always return a transaction or disposition")
    })();
    // Restore even on exhaustion/noncontention failure, before the SQL body can execute.
    // A restoration failure drops any acquired transaction and therefore rolls it back.
    connection
        .busy_timeout(Duration::from_millis(timeout))
        .map_err(map_sql_error)?;
    result
}

fn contention_code(error: &rusqlite::Error) -> Option<i32> {
    match error {
        rusqlite::Error::SqliteFailure(error, _)
            if matches!(
                error.code,
                ffi::ErrorCode::DatabaseBusy | ffi::ErrorCode::DatabaseLocked
            ) =>
        {
            Some(error.extended_code)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests;
