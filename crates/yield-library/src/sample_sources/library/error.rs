use std::path::PathBuf;

use thiserror::Error;

use crate::app_dirs;

use super::tag_vocabulary::ProfileTagVocabularyInputError;

/// Errors returned when operating on the library database.
#[derive(Debug, Error)]
pub enum LibraryError {
    /// Actual global facts query exceeded its SQLite VM/deadline budget.
    #[error("Harvest facts query work exhausted; reduce request or audit schema")]
    HarvestFactsQueryBudget,
    /// Bounded Harvest facts input is invalid or exceeds a fixed budget.
    #[error("invalid or over-budget Harvest facts input")]
    HarvestFactsInput,
    /// Bounded Harvest facts request was canceled.
    #[error("Harvest facts read canceled")]
    HarvestFactsCanceled,
    /// Bounded Harvest facts source or request belongs to another acquisition.
    #[error("stale Harvest facts profile acquisition")]
    HarvestFactsStaleOwner,
    /// Bounded Harvest facts actual issuing source observations no longer match.
    #[error("Harvest facts source changed; reacquire observations")]
    HarvestFactsSourceChanged,
    /// Bounded Harvest facts rows or graph work are malformed or over budget.
    #[error("malformed or over-budget Harvest facts metadata")]
    HarvestFactsMalformed,
    /// Bounded Harvest facts stored metadata does not identify this exact live content.
    #[error("Harvest facts identity uncertain; audit required")]
    HarvestFactsIdentityUncertain,
    /// Explicit authored-state proposal or source context invalid.
    #[error("invalid Harvest authored-state input")]
    HarvestAuthoredInput,
    /// Explicit authored-state acquisition differs from current owner.
    #[error("stale Harvest authored-state acquisition")]
    HarvestAuthoredStaleOwner,
    /// Explicit authored-state operation reused for changed immutable intent.
    #[error("Harvest authored-state replay conflict")]
    HarvestAuthoredReplayConflict,
    /// Explicit authored-state receipt is malformed or ambiguous.
    #[error("malformed Harvest authored-state receipt")]
    HarvestAuthoredMalformedReceipt,
    /// Explicit authored-state existing metadata malformed or ambiguous.
    #[error("malformed Harvest authored-state graph")]
    HarvestAuthoredMalformedGraph,
    /// Explicit authored-state finite retention exhausted without evidence eviction.
    #[error("Harvest authored-state receipt capacity exhausted")]
    HarvestAuthoredReceiptCapacity,
    /// Explicit authored-state commit may be durable; retry the original proposal.
    #[error("Harvest authored-state acknowledgement uncertain; replay original proposal")]
    HarvestAuthoredCommitUncertain,
    /// Writer acquisition failed before any ordinary metadata effect. The proposal may be
    /// resubmitted through its original captured context; no success receipt was issued.
    #[error("ordinary SQLite writer unavailable after {attempts} attempts (code {sqlite_code})")]
    OrdinaryWriteContention {
        /// Number of nonblocking BEGIN IMMEDIATE attempts actually performed.
        attempts: u8,
        /// SQLite extended result code, preserving busy/locked distinctions.
        sqlite_code: i32,
    },
    /// A copy metadata proposal violates exact bounded family/source contracts.
    #[error("invalid Harvest copy input")]
    HarvestCopyInput,
    /// Issuing source/journal/global prerequisite ownership or exact facts failed.
    #[error("Harvest copy evidence unavailable or changed")]
    HarvestCopyEvidence,
    /// A copy request captured another profile acquisition.
    #[error("stale Harvest copy acquisition")]
    HarvestCopyStaleOwner,
    /// An immutable copy key was reused for changed proposal bytes.
    #[error("Harvest copy replay conflict")]
    HarvestCopyReplayConflict,
    /// Retained copy receipt types/canonical bytes/hash/result are invalid.
    #[error("malformed Harvest copy receipt")]
    HarvestCopyMalformedReceipt,
    /// Existing graph facts are ambiguous, malformed or unbounded.
    #[error("malformed Harvest copy graph")]
    HarvestCopyMalformedGraph,
    /// Finite ledger retention cannot accept another immutable result.
    #[error("Harvest copy receipt capacity exhausted")]
    HarvestCopyReceiptCapacity,
    /// A committed copy metadata result lost valid ownership before acknowledgement.
    #[error("Harvest copy commit acknowledgement uncertain; replay original proposal")]
    HarvestCopyCommitUncertain,

    /// The retained SQLite owner cannot admit an ordinary WAL-growing mutation.
    #[error(transparent)]
    WalAdmission(#[from] crate::WalAdmissionError),
    /// An ordinary snapshot proposal is older than this acquisition's last committed one.
    #[error("global snapshot revision {proposed} is older than committed {committed}")]
    GlobalSnapshotStaleRevision {
        /// Supplied acquisition-local proposal revision.
        proposed: u64,
        /// Last committed acquisition-local proposal revision.
        committed: u64,
    },
    /// One snapshot revision was reused for a different immutable payload.
    #[error("global snapshot payload conflict at revision {revision}")]
    GlobalSnapshotPayloadConflict {
        /// Conflicting acquisition-local proposal revision.
        revision: u64,
    },
    /// Bounded reference command or historical/current source scopes are invalid.
    #[error("invalid cross-source reference input")]
    ReferenceInput,
    /// Captured acquisition differs from the current retained profile.
    #[error("stale reference profile owner")]
    ReferenceStaleOwner,
    /// Complete reference observation is malformed or exceeds its finite scope budget.
    #[error("invalid or over-budget reference snapshot")]
    ReferenceSnapshotInvalid,
    /// Immutable command differs under the same operation/work obligation.
    #[error("reference replay conflict")]
    ReferenceReplayConflict,
    /// Receipt evidence is malformed or ambiguous.
    #[error("malformed reference receipt")]
    ReferenceMalformedReceipt,
    /// Ledger capacity is exhausted; existing replay evidence remains retained.
    #[error("reference receipt capacity reached")]
    ReferenceReceiptCapacity,
    /// Source ownership changed after commit: recover exact durable identity before proceeding.
    #[error("reference commit ownership uncertain; disposition may be committed")]
    ReferenceCommitUncertain,
    /// Live source read context failed; no new commit unless explicitly reported uncertain.
    #[error(transparent)]
    ReferenceSource(#[from] crate::sample_sources::db::SourceReferenceFenceError),
    /// Captured ordinary mutation belongs to a different acquisition; no transaction ran.
    #[error("stale global mutation acquisition")]
    GlobalMutationStaleAcquisition,
    /// Completion differs from the originally captured proposal.
    #[error("global mutation completion context mismatch")]
    GlobalMutationCompletionMismatch,
    /// Operation-link request violates a bounded endpoint/identity contract.
    #[error("invalid operation-link input")]
    OperationLinkInput,
    /// Request acquisition does not match the retained profile owner.
    #[error("stale operation-link profile owner")]
    OperationLinkStaleOwner,
    /// Retained evidence differs under the same immutable link key.
    #[error("operation-link replay conflict")]
    OperationLinkReplayConflict,
    /// Retained receipt is malformed or ambiguous.
    #[error("malformed operation-link receipt")]
    OperationLinkMalformedReceipt,
    /// Bounded receipt budget exhausted; no replay evidence is evicted.
    #[error("operation-link receipt capacity reached")]
    OperationLinkReceiptCapacity,
    /// An operation-scoped registry command failed its bounded identity contract.
    #[error("invalid source-registry participant input: {field}")]
    SourceRegistryParticipantInput {
        /// Invalid bounded field.
        field: &'static str,
    },
    /// The supplied profile acquisition is stale; no participant effect was attempted.
    #[error("source-registry participant profile owner is stale")]
    SourceRegistryParticipantStaleOwner,
    /// A retained operation receipt has another immutable command identity.
    #[error("source-registry participant replay conflicts with retained receipt")]
    SourceRegistryParticipantReplayConflict,
    /// The bounded receipt ledger cannot retain another operation safely.
    #[error("source-registry participant receipt capacity reached")]
    SourceRegistryParticipantReceiptCapacity,
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
    /// The retained source registry exceeded its descriptor count bound.
    #[error("source registry contains too many descriptors ({actual}, max {limit})")]
    SourceRegistryTooManyDescriptors {
        /// Maximum accepted descriptor count.
        limit: usize,
        /// Observed descriptor count.
        actual: usize,
    },
    /// A retained source registry payload exceeded its aggregate bound.
    #[error("source registry payload exceeds {limit} bytes ({actual})")]
    SourceRegistryTooLarge {
        /// Maximum accepted encoded or accounted size.
        limit: usize,
        /// Observed payload size.
        actual: usize,
    },
    /// One retained source descriptor or path field exceeded its bound.
    #[error("source registry descriptor {index} field {field} exceeds {limit} bytes ({actual})")]
    SourceRegistryFieldTooLarge {
        /// Descriptor index.
        index: usize,
        /// Field whose bound was exceeded.
        field: &'static str,
        /// Maximum accepted field size.
        limit: usize,
        /// Observed field size.
        actual: usize,
    },
    /// A retained source identity exceeded the shared journal identity bound.
    #[error("source registry descriptor {index} source identity exceeds {limit} bytes ({actual})")]
    SourceRegistrySourceIdTooLarge {
        /// Descriptor index.
        index: usize,
        /// Maximum accepted identity size.
        limit: usize,
        /// Observed identity size.
        actual: usize,
    },
    /// A retained source registry row or metadata value was malformed.
    #[error("source registry data is malformed in {field}")]
    SourceRegistryMalformed {
        /// Field or storage key containing malformed data.
        field: &'static str,
    },
    /// A read-only global library database did not expose the compatible schema.
    #[error("global library read-only schema is incompatible: {reason}")]
    ReadOnlySchemaIncompatible {
        /// Stable reason for rejecting the observation.
        reason: &'static str,
    },
    /// The bounded read-only observation exceeded its cooperative execution budget.
    #[error("global library read-only observation exceeded its budget")]
    ReadOnlyObservationBudgetExceeded,
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
