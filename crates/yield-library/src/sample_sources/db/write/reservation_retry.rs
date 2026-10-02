//! Finite source writer acquisition. Never replay a transaction body or commit.
use std::time::Duration;

use rusqlite::{Transaction, TransactionBehavior, ffi};

use super::super::{SourceDatabase, SourceDbError, util::map_sql_error};

const MAX_ATTEMPTS: u8 = 3;

pub(super) fn begin(database: &SourceDatabase) -> Result<Transaction<'_>, SourceDbError> {
    begin_with_backoff(database, |attempt| {
        std::thread::sleep(Duration::from_millis(u64::from(attempt) * 5));
    })
}

#[derive(Default)]
struct Observation {
    attempts: u8,
    retries: u8,
    sqlite_code: i32,
    outcome: &'static str,
}

fn begin_with_backoff(
    database: &SourceDatabase,
    mut backoff: impl FnMut(u8),
) -> Result<Transaction<'_>, SourceDbError> {
    let connection = &database.connection;
    let mut observation = Observation {
        outcome: "rejected",
        ..Observation::default()
    };
    let result = (|| {
        // Do not disturb a caller's existing transaction, including on rejected ownership.
        if !connection.is_autocommit() {
            return Err(SourceDbError::Unexpected);
        }
        // Preserve the existing single pre-mutation lease preparation boundary.
        database.validate_profile_owned_mutation()?;
        let timeout: u64 = connection
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .map_err(map_sql_error)?;
        connection
            .busy_timeout(Duration::ZERO)
            .map_err(map_sql_error)?;
        let result = (|| {
            for attempt in 1..=MAX_ATTEMPTS {
                if !connection.is_autocommit() {
                    return Err(SourceDbError::Unexpected);
                }
                // Strict revalidation cannot write or renew via the lease control connection.
                observation.outcome = "ownership_rejected";
                revalidate(database)?;
                observation.attempts = attempt;
                match Transaction::new_unchecked(connection, TransactionBehavior::Immediate) {
                    Ok(transaction) => {
                        revalidate(database)?;
                        observation.outcome = "acquired";
                        return Ok(transaction);
                    }
                    Err(error) => {
                        observation.sqlite_code = sqlite_code(&error);
                        let retry = retryable(&error)
                            && connection.is_autocommit()
                            && attempt < MAX_ATTEMPTS;
                        observation.outcome = if retryable(&error) {
                            "busy_exhausted"
                        } else if contention(&error) {
                            "contention_not_retryable"
                        } else {
                            "sqlite_error"
                        };
                        if !retry {
                            return Err(map_sql_error(error));
                        }
                        observation.retries += 1;
                        backoff(attempt);
                    }
                }
            }
            unreachable!("finite attempts return a transaction or exact error")
        })();
        // Restoration precedes returning the acquired transaction to its caller. A failure
        // drops any acquired empty transaction, so its body can never execute.
        if let Err(error) = connection.busy_timeout(Duration::from_millis(timeout)) {
            observation.outcome = "timeout_restore_failed";
            observation.sqlite_code = sqlite_code(&error);
            return Err(map_sql_error(error));
        }
        result
    })();
    tracing::info!(target: "yield::source_writer_acquisition",
        attempts = observation.attempts, retries = observation.retries,
        sqlite_code = observation.sqlite_code, outcome = observation.outcome);
    result
}

fn revalidate(database: &SourceDatabase) -> Result<(), SourceDbError> {
    if let (Some(profile), Some(_)) = (&database.profile_guard, &database.database_file_binding) {
        return database.validate_profile_owned_open(profile);
    }
    // Retain the repository's explicit unowned/test-support fixture behavior. Normal builds
    // reject missing ownership here just as at the initial mutation boundary above.
    database.validate_profile_owned_mutation()
}

fn sqlite_code(error: &rusqlite::Error) -> i32 {
    match error {
        rusqlite::Error::SqliteFailure(error, _) => error.extended_code,
        _ => 0,
    }
}
fn retryable(error: &rusqlite::Error) -> bool {
    sqlite_code(error) == ffi::SQLITE_BUSY
}
fn contention(error: &rusqlite::Error) -> bool {
    matches!(error, rusqlite::Error::SqliteFailure(error, _)
        if matches!(error.code, ffi::ErrorCode::DatabaseBusy | ffi::ErrorCode::DatabaseLocked))
}

#[cfg(test)]
mod tests;
