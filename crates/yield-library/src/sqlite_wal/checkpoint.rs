//! Best-effort maintenance observations on the connection's retained WAL.
use std::cell::Cell;
use std::time::{Duration, Instant};

use rusqlite::Connection;

use super::{WAL_SOFT_WATERMARK_BYTES, WalAdmissionError, retained_wal_bytes};

const MIN_INTERVAL: Duration = Duration::from_secs(15);

/// One timestamp per retained connection; no pathname registry survives owner retirement.
#[derive(Default)]
pub(crate) struct CheckpointPolicy {
    last_attempt: Cell<Option<Instant>>,
}

#[derive(Debug)]
enum Observation {
    ActiveTransaction,
    Unavailable(WalAdmissionError),
    BelowThreshold,
    Throttled,
    Attempt {
        before: u64,
        after: Result<u64, WalAdmissionError>,
        frames: rusqlite::Result<Frames>,
        elapsed: Duration,
    },
}

#[derive(Debug)]
struct Frames {
    busy: bool,
    log: i64,
    checkpointed: i64,
}

impl CheckpointPolicy {
    #[cfg(test)]
    pub(crate) fn has_attempted(&self) -> bool {
        self.last_attempt.get().is_some()
    }

    pub(crate) fn maybe_checkpoint(
        &self,
        connection: &Connection,
        database_kind: &'static str,
        role: &'static str,
    ) {
        match self.observe(connection, Instant::now(), WAL_SOFT_WATERMARK_BYTES) {
            Observation::Attempt {
                before,
                after,
                frames,
                elapsed,
            } => {
                let after_bytes = after.as_ref().ok().copied();
                let after_unavailable = after.err().map(|error| error.to_string());
                match frames {
                    Ok(frames) => tracing::info!(
                        target: "perf::source_db", action = "wal_checkpoint",
                        database_kind, role, mode = "passive", wal_bytes_before = before,
                        wal_bytes_after = ?after_bytes, observation_unavailable = ?after_unavailable,
                        busy = frames.busy, log_frames = frames.log,
                        checkpointed_frames = frames.checkpointed,
                        complete = !frames.busy && frames.log >= 0 && frames.log == frames.checkpointed,
                        elapsed_ms = elapsed.as_millis() as u64,
                        "Observed passive WAL checkpoint"
                    ),
                    Err(error) => tracing::warn!(
                        target: "perf::source_db", action = "wal_checkpoint",
                        database_kind, role, mode = "passive", wal_bytes_before = before,
                        wal_bytes_after = ?after_bytes, observation_unavailable = ?after_unavailable,
                        sqlite_code = ?error.sqlite_error().map(|error| error.extended_code),
                        elapsed_ms = elapsed.as_millis() as u64,
                        "Passive WAL checkpoint failed"
                    ),
                }
            }
            Observation::Unavailable(error) => tracing::debug!(
                target: "perf::source_db", action = "wal_checkpoint_observation_unavailable",
                database_kind, role, reason = %error, "Skipping unmeasured WAL maintenance"
            ),
            _ => {}
        }
    }

    fn observe(&self, connection: &Connection, now: Instant, threshold: u64) -> Observation {
        // Maintenance is post-commit only. Never checkpoint through an active reader/writer txn.
        if !connection.is_autocommit() {
            return Observation::ActiveTransaction;
        }
        if self
            .last_attempt
            .get()
            .is_some_and(|last| now.saturating_duration_since(last) < MIN_INTERVAL)
        {
            return Observation::Throttled;
        }
        let before = match retained_wal_bytes(connection) {
            Ok(bytes) => bytes,
            Err(error) => return Observation::Unavailable(error),
        };
        if before < threshold {
            return Observation::BelowThreshold;
        }
        self.last_attempt.set(Some(now));
        let started = Instant::now();
        let frames = connection.query_row("PRAGMA main.wal_checkpoint(PASSIVE)", [], |row| {
            Ok(Frames {
                busy: row.get::<_, i64>(0)? != 0,
                log: row.get(1)?,
                checkpointed: row.get(2)?,
            })
        });
        Observation::Attempt {
            before,
            after: retained_wal_bytes(connection),
            frames,
            elapsed: started.elapsed(),
        }
    }
}

#[cfg(test)]
mod tests;
