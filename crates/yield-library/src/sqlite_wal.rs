//! Shared WAL policy helpers for SQLite-backed Yield databases.
//!
//! The source and library databases both run in WAL mode so readers can stay
//! responsive while scan, enqueue, and analysis writers churn in the
//! background. This module makes the policy explicit instead of relying on
//! SQLite defaults by:
//!
//! - setting a conservative auto-checkpoint threshold
//! - capping retained post-checkpoint journal size
//! - attempting throttled passive checkpoints only after the WAL has already
//!   grown beyond the normal steady-state target

mod admission;
pub use admission::{WAL_HARD_WATERMARK_BYTES, WAL_SOFT_WATERMARK_BYTES, WalAdmissionError};
pub(crate) use admission::{enforce_wal_admission, retained_database_bytes, retained_wal_bytes};

mod checkpoint;
pub(crate) use checkpoint::CheckpointPolicy;

use rusqlite::Connection;

/// SQL fragment that applies the shared WAL-health settings for write-capable DBs.
pub(crate) const WORKLOAD_WAL_PRAGMAS_SQL: &str = "PRAGMA wal_autocheckpoint=4096;
                 PRAGMA journal_size_limit=67108864;";

/// Apply the shared write-capable WAL policy to one SQLite connection.
pub(crate) fn apply_workload_wal_pragmas(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(WORKLOAD_WAL_PRAGMAS_SQL)?;
    connection.pragma_update(None, "wal_autocheckpoint", 4096_i64)?;
    connection.pragma_update(None, "journal_size_limit", 67_108_864_i64)
}
