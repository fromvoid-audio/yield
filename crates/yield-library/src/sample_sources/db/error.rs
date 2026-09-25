use std::path::PathBuf;

use thiserror::Error;

use super::types::SourceDirectoryTruthUnavailableReason;

/// Typed fail-closed errors from the directory-truth publication contract.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum SourceDirectoryTruthError {
    /// A generation number is not a valid positive source-local generation.
    #[error("directory truth generation {generation} is invalid")]
    InvalidGeneration {
        /// Invalid generation supplied by the caller.
        generation: u64,
    },
    /// An entry count cannot be represented safely by the source database.
    #[error("directory truth entry count {count} is invalid")]
    InvalidEntryCount {
        /// Invalid entry count supplied by the caller.
        count: u64,
    },
    /// The caller's expected source revision no longer matches the writer transaction.
    #[error("directory truth expected source revision {expected}, found {actual}")]
    StaleRevision {
        /// Revision supplied by the caller.
        expected: u64,
        /// Revision observed under the writer lock.
        actual: u64,
    },
    /// The requested generation does not exist.
    #[error("directory truth generation {generation} does not exist")]
    GenerationMissing {
        /// Generation requested by the caller.
        generation: u64,
    },
    /// The staged generation does not contain its declared number of entries.
    #[error(
        "directory truth generation {generation} is incomplete: expected {expected}, staged {staged}"
    )]
    Incomplete {
        /// Generation being finalized.
        generation: u64,
        /// Declared entry count.
        expected: u64,
        /// Persisted entry count.
        staged: u64,
    },
    /// A batch contains the same normalized path more than once.
    #[error("directory truth path is duplicated: {path}")]
    DuplicatePath {
        /// Duplicate normalized path.
        path: PathBuf,
    },
    /// A generation already contains the same path.
    #[error("directory truth path already exists in the generation: {path}")]
    ExistingPath {
        /// Existing normalized path.
        path: PathBuf,
    },
    /// A batch or generation contains the same stable directory identity more than once.
    #[error("directory truth directory identity is duplicated: {identity}")]
    DuplicateDirectoryIdentity {
        /// Duplicate stable identity.
        identity: String,
    },
    /// A requested generation collides with a generation in another lifecycle state.
    #[error("directory truth generation {generation} has already been published or retired")]
    GenerationCollision {
        /// Colliding generation.
        generation: u64,
    },
    /// A batch would exceed the generation's declared entry count.
    #[error("directory truth generation {generation} would exceed its declared entry count")]
    EntryCountExceeded {
        /// Generation receiving the batch.
        generation: u64,
    },
    /// A batch exceeds the bounded staging limit.
    #[error("directory truth staging batch exceeds the bounded limit")]
    BatchTooLarge,
    /// A path or identity cannot be persisted without losing its meaning.
    #[error("directory truth entry is invalid: {path}")]
    InvalidPath {
        /// Invalid path supplied by the caller.
        path: PathBuf,
    },
    /// A directory identity is empty or contains unsupported control data.
    #[error("directory truth directory identity is invalid")]
    InvalidDirectoryIdentity,
    /// The database shape is not safe to write.
    #[error("directory truth schema is unavailable or malformed")]
    SchemaUnavailable,
    /// A read cursor belongs to a different active generation or revision.
    #[error("directory truth cursor is stale")]
    StaleCursor,
    /// Persisted directory state requires an audit before mutation.
    #[error("directory truth requires an audit: {reason:?}")]
    RequiresAudit {
        /// Fail-closed reason.
        reason: SourceDirectoryTruthUnavailableReason,
    },
}

/// Typed failures from the retained identity proof for a profile-owned source database.
///
/// These outcomes are intentionally separate from the older path-policy error. A replacement or
/// unavailable identity primitive must remain distinguishable to callers; the textual diagnostic
/// is supplementary context and never a takeover or admission decision.
#[derive(Debug, Error)]
pub enum SourceDatabaseIdentityError {
    /// A filesystem operation failed while checking the retained database path.
    #[error("could not inspect source database identity at {path}: {source}")]
    Io {
        /// Path involved in the failed identity check.
        path: PathBuf,
        /// Underlying filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// The retained database path or opened handle is not a regular file.
    #[error("source database identity path is not a regular file: {path}")]
    NotRegular {
        /// Path whose file type was rejected.
        path: PathBuf,
    },
    /// The retained database pathname now identifies a different file or no file.
    #[error("source database file was replaced: {path}")]
    Replaced {
        /// Path whose identity changed.
        path: PathBuf,
    },
    /// The host or SQLite VFS cannot provide the required identity primitive.
    #[error("source database identity is unavailable at {path}: {reason}")]
    Unavailable {
        /// Path being checked.
        path: PathBuf,
        /// Bounded diagnostic describing the unavailable primitive.
        reason: String,
    },
    /// SQLite's native connection handle identifies a different file.
    #[error("source database connection identity mismatch at {path}: {reason}")]
    ConnectionMismatch {
        /// Path being checked.
        path: PathBuf,
        /// Bounded diagnostic describing the mismatch.
        reason: String,
    },
    /// A source database WAL or shared-memory sidecar violates the existing path policy.
    #[error("source database sidecar is unsafe at {path}: {reason}")]
    UnsafeSidecar {
        /// Sidecar path rejected by the source path policy.
        path: PathBuf,
        /// Stable policy reason.
        reason: &'static str,
    },
}

/// Errors returned when managing a source database.
#[derive(Debug, Error)]
pub enum SourceDbError {
    /// Writable source database access requires an already-acquired profile ownership capability.
    #[error("profile ownership is required for writable source database access")]
    ProfileOwnershipRequired,
    /// The provided root path is not a directory.
    #[error("Source folder is not a directory: {0}")]
    InvalidRoot(PathBuf),
    /// SQLite query failed.
    #[error("Database query failed: {0}")]
    Sql(#[from] rusqlite::Error),
    /// Failed to create a parent directory.
    #[error("Could not write to {path}: {source}")]
    CreateDir {
        /// Path that could not be created.
        path: PathBuf,
        /// Underlying IO error.
        source: std::io::Error,
    },
    /// Provided path was not relative to the source root.
    #[error("Path must be relative to the source root: {0}")]
    PathMustBeRelative(PathBuf),
    /// Provided path contained disallowed components or was empty.
    #[error("Path contains invalid relative components: {0}")]
    InvalidRelativePath(PathBuf),
    /// A relative path could not be represented without losing filesystem identity.
    #[error("Path is not valid Unicode and cannot be persisted safely: {0}")]
    NonUnicodeRelativePath(PathBuf),
    /// Database is locked or busy.
    #[error("Database is busy, please retry")]
    Busy,
    /// A caller canceled an in-progress database operation.
    #[error("Database operation canceled")]
    Canceled,
    /// SQLite returned an unexpected result.
    #[error("SQLite returned an unexpected result")]
    Unexpected,
    /// Curation tag facts could not be proven for one requested live path.
    #[error("Curation tag facts unavailable for {path}")]
    CurationTagFactsUnavailable {
        /// Path relative to the source root.
        path: PathBuf,
    },
    /// A bounded Curation facts batch exceeded its tag-assignment work limit.
    #[error("Curation tag facts exceed {max} assignments in one batch")]
    CurationTagFactsTooManyAssignments {
        /// Maximum tag-assignment rows read per batch.
        max: usize,
    },
    /// Directory-truth storage rejected a stale, malformed, or conflicting operation.
    #[error(transparent)]
    DirectoryTruth(#[from] SourceDirectoryTruthError),
    /// Provided tag text cannot be normalized to a non-empty identity.
    #[error("Tag label cannot be empty")]
    EmptyTagLabel,
    /// A curation tag label exceeded the bounded UTF-8 byte length.
    #[error("Curation tag label is {actual} UTF-8 bytes; maximum is {max}")]
    TagLabelTooLong {
        /// Maximum accepted UTF-8 byte length.
        max: usize,
        /// Supplied UTF-8 byte length.
        actual: usize,
    },
    /// A curation tag label contained an embedded NUL byte.
    #[error("Curation tag label contains an embedded NUL byte")]
    TagLabelContainsNul,
    /// A source-local tag row identifier was not a positive database identity.
    #[error("Source tag id {tag_id} is invalid; ids must be positive")]
    InvalidTagId {
        /// Invalid source-local tag row identifier.
        tag_id: i64,
    },
    /// A positive source-local tag row identifier did not exist in the dictionary.
    #[error("Source tag id {tag_id} does not exist")]
    TagIdNotFound {
        /// Missing source-local tag row identifier.
        tag_id: i64,
    },
    /// A bounded tag-page request contained too many selected paths.
    #[error("Tag page selected {actual} paths; maximum is {max}")]
    TagPageTooManyPaths {
        /// Maximum number of selected paths accepted by one page request.
        max: usize,
        /// Number of paths supplied by the caller.
        actual: usize,
    },
    /// A bounded tag-page request exceeded its aggregate selected-path byte budget.
    #[error("Tag page selected paths use {actual} bytes; maximum is {max}")]
    TagPagePathBytesTooLarge {
        /// Maximum aggregate UTF-8/path byte length accepted by one page request.
        max: usize,
        /// Aggregate path byte length supplied by the caller.
        actual: usize,
    },
    /// A bounded selected-tag page request contained too many source-tag ids.
    #[error("Tag page selected {actual} tag ids; maximum is {max}")]
    TagPageTooManyTagIds {
        /// Maximum number of source-tag ids accepted by one request.
        max: usize,
        /// Number of source-tag ids supplied by the caller.
        actual: usize,
    },
    /// A bounded tag-page vocabulary prefix exceeded its UTF-8 byte budget.
    #[error("Tag page prefix is {actual} UTF-8 bytes; maximum is {max}")]
    TagPagePrefixTooLong {
        /// Maximum prefix byte length accepted by one page request.
        max: usize,
        /// Prefix byte length supplied by the caller.
        actual: usize,
    },
    /// A bounded tag-page vocabulary prefix contained an embedded NUL byte.
    #[error("Tag page prefix contains an embedded NUL byte")]
    TagPagePrefixContainsNul,
    /// A source tag filter request contained too many required tokens.
    #[error("Tag filter contains {actual} tokens; maximum is {max}")]
    TagFilterTooManyTokens {
        /// Maximum number of required tokens accepted by one filter request.
        max: usize,
        /// Number of tokens supplied by the caller.
        actual: usize,
    },
    /// One source tag filter token exceeded its UTF-8 byte budget.
    #[error("Tag filter token is {actual} UTF-8 bytes; maximum is {max}")]
    TagFilterTokenTooLong {
        /// Maximum UTF-8 byte length of one required token.
        max: usize,
        /// UTF-8 byte length supplied by the caller.
        actual: usize,
    },
    /// A source tag filter request exceeded its aggregate UTF-8 byte budget.
    #[error("Tag filter input is {actual} UTF-8 bytes; maximum is {max}")]
    TagFilterInputTooLarge {
        /// Maximum aggregate UTF-8 byte length accepted by one filter request.
        max: usize,
        /// Aggregate UTF-8 byte length supplied by the caller.
        actual: usize,
    },
    /// A bounded display-label request contained too many paths.
    #[error("Display-label request selected {actual} paths; maximum is {max}")]
    DisplayLabelTooManyPaths {
        /// Maximum number of paths accepted by one display-label request.
        max: usize,
        /// Number of raw paths supplied by the caller.
        actual: usize,
    },
    /// A bounded display-label request exceeded its aggregate path-byte budget.
    #[error("Display-label request paths use {actual} bytes; maximum is {max}")]
    DisplayLabelPathBytesTooLarge {
        /// Maximum aggregate raw plus normalized path-byte length accepted by one request.
        max: usize,
        /// Aggregate path-byte length supplied by the caller.
        actual: usize,
    },
    /// A display-label request observed a source revision different from its fence.
    #[error("Display-label request expected source revision {expected}, found {actual}")]
    DisplayLabelStaleRevision {
        /// Revision supplied by the caller.
        expected: u64,
        /// Revision observed by the read transaction.
        actual: u64,
    },
    /// Read-only mode requires an existing database file.
    #[error("Read-only source DB mode requires an existing database file: {0}")]
    ReadOnlyDatabaseMissing(PathBuf),
    /// Source database path policy rejected an unsafe local database path.
    #[error("Unsafe source database path {path}: {reason}")]
    UnsafeSourceDatabasePath {
        /// Path rejected by the source DB path policy.
        path: PathBuf,
        /// Stable reason suitable for user-facing status and diagnostics.
        reason: &'static str,
    },
    /// Failed to inspect a source database path before trusting it.
    #[error("Could not inspect source database path {path}: {source}")]
    InspectSourceDatabasePath {
        /// Path that could not be inspected.
        path: PathBuf,
        /// Underlying IO error.
        source: std::io::Error,
    },
    /// Failed to resolve a source database path before trusting it.
    #[error("Could not resolve source database path {path}: {source}")]
    ResolveSourceDatabasePath {
        /// Path that could not be resolved.
        path: PathBuf,
        /// Underlying IO error.
        source: std::io::Error,
    },
    /// Failed to resolve the app-owned metadata folder for a protected source.
    #[error("Could not resolve external metadata storage for {path}: {source}")]
    ExternalMetadataRoot {
        /// Source root whose external metadata folder was requested.
        path: PathBuf,
        /// Underlying application directory error.
        source: crate::app_dirs::AppDirError,
    },
    /// The retained profile capability could not safely host an external catalog database.
    #[error("Profile-owned catalog database root is unavailable: {0}")]
    ProfileOwnership(#[from] crate::app_dirs::ProfileOwnershipError),
    /// A retained profile-owned source database identity check failed.
    #[error(transparent)]
    DatabaseIdentity(#[from] SourceDatabaseIdentityError),
    /// A retained physical-source lease could not be acquired or validated.
    #[error(transparent)]
    SourceLease(#[from] super::source_lease::SourceLeaseError),
    /// Failed to move a source DB from its legacy filename to the current filename.
    #[error("Could not migrate source database from {from} to {to}: {source}")]
    RenameLegacyDatabase {
        /// Legacy source DB path.
        from: PathBuf,
        /// Current source DB path.
        to: PathBuf,
        /// Underlying IO error.
        source: std::io::Error,
    },
    /// A profile-owned open found a legacy database that would require an ambient pathname rename.
    #[error("Profile-owned source database requires a bound legacy migration from {from} to {to}")]
    LegacyDatabaseMigrationRequiresBoundRoot {
        /// Legacy source DB path retained for a future descriptor-bound migration.
        from: PathBuf,
        /// Current source DB path that was not created or modified.
        to: PathBuf,
    },
}
