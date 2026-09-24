#![deny(missing_docs)]
#![deny(warnings)]

//! Shared application-directory and library-storage helpers.

/// Application directory helpers anchored to the `.yield` root.
pub mod app_dirs;
/// Shared structured debug diagnostics helpers for storage-owned seams.
pub mod diagnostics;
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
/// Optional SQLite extension loader shared by storage code.
pub mod sqlite_ext;
mod sqlite_wal;
/// Process-global test runtime isolation helpers.
#[cfg(any(test, feature = "test-support"))]
pub mod test_runtime;
/// Shared signed Unix timestamp conversion helpers.
pub mod timestamps;
