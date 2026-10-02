#![deny(missing_docs)]
#![deny(warnings)]

//! Shared application-directory and library-storage helpers.

/// Application directory helpers anchored to the `.yield` root.
pub mod app_dirs;
/// Shared structured debug diagnostics helpers for storage-owned seams.
pub mod diagnostics;
#[cfg(not(windows))]
mod directory_sync;
/// Non-authoritative durability qualification vocabulary and evidence matrix.
pub mod durability_matrix;
/// Pre-intent durable capacity discovery and accounting.
pub mod durable_capacity;
mod env_flags;
/// Bounded owner-thread command and receipt boundary for future file effects.
pub mod file_operation_owner;
/// Stable cross-platform filesystem-object identity helpers.
pub mod filesystem_identity;
/// Storage-neutral profile operation identities, intents, phases, and dispositions.
pub mod operation_journal;
/// Sample-source identifiers and persistent storage helpers.
pub mod sample_sources;
mod sqlite_budget;
/// Optional SQLite extension loader shared by storage code.
pub mod sqlite_ext;
#[cfg(target_os = "linux")]
mod sqlite_linux;
mod sqlite_wal;
pub use sqlite_wal::{WAL_HARD_WATERMARK_BYTES, WAL_SOFT_WATERMARK_BYTES, WalAdmissionError};
/// Process-global test runtime isolation helpers.
#[cfg(any(test, feature = "test-support"))]
pub mod test_runtime;
/// Shared signed Unix timestamp conversion helpers.
pub mod timestamps;
