use std::path::PathBuf;

use thiserror::Error;

use crate::app_dirs;

use super::tag_vocabulary::ProfileTagVocabularyInputError;

/// Errors returned when operating on the library database.
#[derive(Debug, Error)]
pub enum LibraryError {
    /// Writable library database access requires an already-acquired profile ownership capability.
    #[error("profile ownership is required for writable library database access")]
    ProfileOwnershipRequired,
    /// No suitable application directory was available.
    #[error("No suitable config directory available for library database")]
    NoConfigDir,
    /// Failed to create the directory for the database file.
    #[error("Could not create library directory {path}: {source}")]
    CreateDir {
        /// Path that could not be created.
        path: PathBuf,
        /// Underlying IO error.
        source: std::io::Error,
    },
    /// A database, WAL, or SHM entry was not safe to hand to SQLite.
    #[error("Library database path validation failed at {path}: {reason}")]
    DatabasePathValidation {
        /// Entry that failed no-follow validation.
        path: PathBuf,
        /// Stable validation failure description.
        reason: String,
    },
    /// Failed to resolve the configured persistence profile.
    #[error("Invalid library persistence profile '{profile}'")]
    InvalidProfile {
        /// Rejected profile name.
        profile: String,
    },
    /// The profile ownership capability could not be used for a library operation.
    #[error(transparent)]
    ProfileOwnership(#[from] app_dirs::ProfileOwnershipError),
    /// Profile ownership changed while an accepted library command was in flight.
    #[error("profile ownership changed at {path}: {reason}")]
    ProfileOwnershipChanged {
        /// Profile root whose ownership boundary changed.
        path: PathBuf,
        /// Stable reason for the fail-closed result.
        reason: String,
    },
    /// Failed to open or query the database.
    #[error("Library database query failed: {0}")]
    Sql(#[from] rusqlite::Error),
    /// Failed to deserialize JSON metadata from the DB.
    #[error("Library metadata parse failed: {0}")]
    Json(#[from] serde_json::Error),
    /// Failed to deserialize one named JSON metadata value from the DB.
    #[error("Library metadata key '{key}' parse failed: {source}")]
    MetadataJson {
        /// Metadata key that contained invalid JSON.
        key: &'static str,
        /// Underlying JSON error.
        source: serde_json::Error,
    },
    /// A bounded metadata payload exceeded the owner limit before it was read into memory.
    #[error("Library metadata key '{key}' exceeds the {limit}-byte limit ({actual} bytes)")]
    MetadataTooLarge {
        /// Metadata key whose payload was too large.
        key: &'static str,
        /// Maximum accepted UTF-8 byte length.
        limit: usize,
        /// Stored payload byte length.
        actual: usize,
    },
    /// A bounded metadata payload was not valid UTF-8.
    #[error("Library metadata key '{key}' is not valid UTF-8")]
    MetadataInvalidUtf8 {
        /// Metadata key containing invalid bytes.
        key: &'static str,
    },
    /// Profile vocabulary input failed a canonical spelling or category policy check.
    #[error("invalid profile tag vocabulary input: {0}")]
    InvalidProfileTagVocabularyInput(#[from] ProfileTagVocabularyInputError),
    /// A profile vocabulary row failed its bounded authoritative representation checks.
    #[error("profile tag vocabulary data is malformed: {reason}")]
    ProfileTagVocabularyMalformedData {
        /// Stable reason for rejecting the corrupt row.
        reason: &'static str,
    },
    /// The profile vocabulary revision did not contain a bounded unsigned integer.
    #[error("profile tag vocabulary revision metadata is malformed")]
    ProfileTagVocabularyRevisionMalformed,
    /// A profile vocabulary write observed a different revision than requested.
    #[error("profile tag vocabulary revision is stale: expected {expected}, actual {actual}")]
    ProfileTagVocabularyStaleRevision {
        /// Revision supplied by the caller.
        expected: u64,
        /// Revision observed in the database.
        actual: u64,
    },
    /// A vocabulary continuation was used with a different revision or literal prefix.
    #[error("profile tag vocabulary cursor does not match its revision or prefix")]
    ProfileTagVocabularyCursorMismatch,
    /// The monotonic profile vocabulary revision could not be incremented.
    #[error("profile tag vocabulary revision overflowed")]
    ProfileTagVocabularyRevisionOverflow,
    /// A retained source descriptor contained a present but unsupported enum value.
    #[error("Retained source '{root}' has invalid {field} value '{value}'")]
    InvalidRetainedSourceDescriptor {
        /// Retained source root used to identify the corrupt record.
        root: PathBuf,
        /// Descriptor field containing the unsupported value.
        field: &'static str,
        /// Unsupported stored value.
        value: String,
    },
}

pub(super) fn map_sql_error(err: rusqlite::Error) -> LibraryError {
    LibraryError::Sql(err)
}

pub(super) fn map_app_dir_error(error: app_dirs::AppDirError) -> LibraryError {
    match error {
        app_dirs::AppDirError::NoBaseDir => LibraryError::NoConfigDir,
        app_dirs::AppDirError::CreateDir { path, source } => {
            LibraryError::CreateDir { path, source }
        }
        app_dirs::AppDirError::InvalidProfileName { profile } => {
            LibraryError::InvalidProfile { profile }
        }
    }
}
