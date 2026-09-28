use std::path::PathBuf;

use thiserror::Error;

use crate::sample_sources::SourceDbError;

use super::ScanStats;

/// Errors that can occur while scanning a source folder.
#[derive(Debug, Error)]
pub enum ScanError {
    /// The provided root path is not a directory.
    #[error("Source root is not a directory: {0}")]
    InvalidRoot(PathBuf),
    /// Scan was canceled by the caller.
    #[error("Scan canceled")]
    Canceled,
    /// The catalog's source generation was retired before a bounded write.
    #[error("Source lifecycle changed while scanning (expected {expected}, current {actual:?})")]
    StaleLifecycle {
        /// Generation retained by this scan.
        expected: u64,
        /// Current generation, or none when the source is no longer registered.
        actual: Option<u64>,
    },
    /// The retained source writer rejected its current physical or lease fence.
    #[error("Source writer authority is unavailable: {0}")]
    SourceOwner(String),
    /// A newer source manifest revision invalidated work planned from an older snapshot.
    #[error(
        "Source manifest changed while the scan checkpoint was being finalized \
         (expected revision {expected}, found {actual})"
    )]
    StaleRevision {
        /// Manifest revision from which the work was planned.
        expected: u64,
        /// Current manifest revision observed before commit.
        actual: u64,
    },
    /// Targeted rename recovery exceeded its bounded carry-over candidate set.
    #[error(
        "Targeted rename recovery exceeded the bounded candidate limit ({limit}); \
         authoritative recovery is required"
    )]
    TargetedRenameCandidateOverflow {
        /// Maximum number of carry-over candidates admitted to targeted recovery.
        limit: usize,
    },
    /// The configured source path no longer names the retained root directory.
    #[error("Source root generation changed while scanning: {root}")]
    StaleRootGeneration {
        /// Configured source root whose identity changed or became unavailable.
        root: PathBuf,
    },
    /// The hidden-directory policy changed while the scan was in progress.
    #[error("Source traversal policy changed while the scan was in progress")]
    TraversalPolicyChanged,
    /// A source revision committed before later work stopped.
    #[error("Scan incomplete after committed checkpoint: {error}")]
    Incomplete {
        /// Authoritative checkpoint that callers must publish before retrying.
        committed: Box<ScanStats>,
        /// Error that stopped the remaining work.
        error: String,
    },
    /// Failed to read a file or directory.
    #[error("Failed to read {path}: {source}")]
    Io {
        /// Path that failed to read.
        path: PathBuf,
        /// Underlying IO error.
        source: std::io::Error,
    },
    /// Database operation failed during scan.
    #[error("Database error: {0}")]
    Db(#[from] SourceDbError),
    /// Failed to convert filesystem time metadata.
    #[error("Time conversion failed for {path}")]
    Time {
        /// Path whose timestamp could not be converted.
        path: PathBuf,
    },
}
