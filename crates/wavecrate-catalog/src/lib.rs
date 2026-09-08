#![deny(missing_docs)]
#![deny(warnings)]

//! Profile-owned durable source catalogs.
//!
//! This crate owns one bounded worker and one app-profile database per registered source. It
//! deliberately accepts only scanner-produced authoritative audits; filesystem watcher evidence
//! is not a catalog publication mechanism.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use thiserror::Error;
use wavecrate_library::app_dirs::{ProfileOwnershipError, WritableProfileGuard};
use wavecrate_library::filesystem_identity::stable_filesystem_identity;
use wavecrate_library::sample_sources::db::{
    SOURCE_DIRECTORY_TRUTH_BATCH_LIMIT, SOURCE_DIRECTORY_TRUTH_CLEANUP_LIMIT, SourceDirectoryEntry,
    SourceDirectoryTruthPage, SourceDirectoryTruthState, SourceManifestEntry,
};
use wavecrate_library::sample_sources::reconciliation::{RootIdentity, RootRelativePath};
pub use wavecrate_library::sample_sources::{
    BrowserFileMetadata, SOURCE_TAG_PAGE_MAX_PAGE_SIZE, SOURCE_TAG_PAGE_MAX_PATH_BYTES,
    SOURCE_TAG_PAGE_MAX_PATHS, SOURCE_TAG_PAGE_MAX_PREFIX_BYTES, SourceCurationCommitReceipt,
    SourceCurationUpdate, SourceTagPage, SourceTagPageCursor, SourceTagPageEntry,
};
use wavecrate_library::sample_sources::{SourceDatabase, SourceDbError, SourceId};
use wavecrate_scan::sample_sources::scanner::{
    ScanError, ScanWritePhase, ScanWriter,
    audit_manifest_for_catalog_with_budget_and_progress_and_writer,
    read_wav_display_metadata_for_manifest,
};

mod curation;
mod tags;
mod wav_metadata;

const COMMAND_CAPACITY: usize = 32;
const EVENT_CAPACITY: usize = 64;
const WORKER_IDLE_POLL: Duration = Duration::from_millis(25);
const DIRECTORY_PAGE_LIMIT: usize = 256;
const CATALOG_CHILD_PREFIX: &str = "wavecrate-catalog-";

#[cfg(test)]
struct CatalogTestGate {
    state: Mutex<CatalogTestGateState>,
    wake: Condvar,
}

#[cfg(test)]
struct CatalogTestGateState {
    paused: bool,
    at_boundary: bool,
}

#[cfg(not(test))]
struct CatalogTestGate;

#[cfg(test)]
impl CatalogTestGate {
    fn new() -> Self {
        Self {
            state: Mutex::new(CatalogTestGateState {
                paused: false,
                at_boundary: false,
            }),
            wake: Condvar::new(),
        }
    }

    fn before_receive(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.at_boundary = true;
        self.wake.notify_all();
        while state.paused {
            state = self
                .wake
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        state.at_boundary = false;
        self.wake.notify_all();
    }

    fn pause_next(&self) -> CatalogTestGatePause<'_> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.paused = true;
        while !state.at_boundary {
            state = self
                .wake
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        CatalogTestGatePause {
            gate: self,
            released: false,
        }
    }

    fn release(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.paused = false;
        self.wake.notify_all();
    }
}

#[cfg(test)]
struct CatalogTestGatePause<'a> {
    gate: &'a CatalogTestGate,
    released: bool,
}

#[cfg(test)]
impl CatalogTestGatePause<'_> {
    fn release(mut self) {
        self.released = true;
        self.gate.release();
    }
}

#[cfg(test)]
impl Drop for CatalogTestGatePause<'_> {
    fn drop(&mut self) {
        if !self.released {
            self.gate.release();
        }
    }
}

#[cfg(not(test))]
impl CatalogTestGate {
    fn before_receive(&self) {}
}

/// Maximum number of exact source paths admitted by one lazy WAV metadata request.
///
/// The scanner performs two bounded one-megabyte header passes per entry. Keeping the request
/// below the scanner's aggregate 64 MiB budget guarantees that every admitted path is attempted
/// in one operation and that an omitted result is terminal for the request.
pub const MAX_WAV_METADATA_REQUEST_PATHS: usize = 16;

/// Maximum number of metadata updates admitted by one durable curation command.
pub const MAX_CURATION_UPDATES: usize = curation::MAX_CURATION_UPDATES;

/// Maximum number of library-sized batches admitted by one curation operation.
pub const MAX_CURATION_OPERATION_BATCHES: usize = curation::MAX_CURATION_OPERATION_BATCHES;

/// Maximum number of updates admitted by one curation operation.
pub const MAX_CURATION_OPERATION_UPDATES: usize = curation::MAX_CURATION_OPERATION_UPDATES;

/// Maximum aggregate source-relative path bytes admitted by one curation operation.
pub const MAX_CURATION_OPERATION_PATH_BYTES: usize = curation::MAX_CURATION_OPERATION_PATH_BYTES;

/// Maximum estimated curation operation payload retained across queued outcomes.
pub const MAX_CURATION_OPERATION_RECEIPT_BYTES: usize =
    curation::MAX_CURATION_OPERATION_RECEIPT_BYTES;

/// Maximum UTF-8 byte length of a curation tag label before identity normalization.
pub const MAX_CURATION_TAG_LABEL_BYTES: usize = curation::MAX_CURATION_TAG_LABEL_BYTES;

/// Maximum estimated selected-path input retained by admitted catalog commands.
///
/// The estimate includes each path's encoded bytes and its in-memory `PathBuf` container. It is
/// borrowed while a command remains queued or is being processed, then released when processing
/// ends so tag-page input retention is independent of event and durable-curation credits.
pub const MAX_TAG_PAGE_INPUT_BYTES: usize = 32 * 1024 * 1024;

pub use curation::{
    CatalogCurationRequestError, SourceCurationOperationFailure, SourceCurationOperationFence,
    SourceCurationOperationOutcome,
};

/// A typed operation accepted by the catalog worker.
#[derive(Debug)]
pub enum CatalogCommand {
    /// Register or replace one source lifecycle generation.
    Register {
        /// Stable source identifier.
        source_id: SourceId,
        /// Scanner-canonicalized absolute source root.
        canonical_root: PathBuf,
        /// Monotonic source lifecycle generation.
        lifecycle_generation: u64,
    },
    /// Run an authoritative scanner audit for one registered generation.
    RequestAudit {
        /// Stable source identifier.
        source_id: SourceId,
        /// Generation that must still own the source when the audit publishes.
        lifecycle_generation: u64,
    },
    /// Read bounded WAV header metadata for exact paths in the retained source snapshot.
    ///
    /// A request whose lifecycle has no current admission, including a superseded, removed, or
    /// never-registered source, is discarded by the worker's fence without a completion event.
    RequestWavMetadata {
        /// Stable source identifier.
        source_id: SourceId,
        /// Generation that must still own the source when the request publishes.
        lifecycle_generation: u64,
        /// Retained catalog revision required by the request.
        expected_revision: u64,
        /// Caller-assigned request identifier.
        request_id: u64,
        /// Source-relative paths to attempt. The worker admits at most
        /// [`MAX_WAV_METADATA_REQUEST_PATHS`] paths per request.
        relative_paths: Vec<PathBuf>,
    },
    /// Read one bounded page of source tags at an exact retained revision.
    ///
    /// Tag pages are ephemeral reads. A superseded, removed, or canceled lifecycle may discard
    /// the request without publishing a completion event.
    RequestTagPage {
        /// Stable source identifier.
        source_id: SourceId,
        /// Generation that must still own the source when the request publishes.
        lifecycle_generation: u64,
        /// Retained catalog revision required by the request.
        expected_revision: u64,
        /// Caller-assigned query identifier.
        request_id: u64,
        /// Bounded tag query.
        query: CatalogTagPageQuery,
        /// Opaque cursor returned by the preceding page, if any.
        after_cursor: Option<SourceTagPageCursor>,
        /// Requested page size. Values outside the library bound are rejected at admission.
        page_limit: usize,
    },
    /// Apply one durable, revision-fenced source curation batch.
    ///
    /// Once admitted, this command is drained to a terminal completion even when a later remove,
    /// replacement, cancellation, or worker shutdown invalidates ephemeral catalog work. An
    /// accepted command submitted before its first complete snapshot may still complete with
    /// [`CatalogError::NotRegistered`] or [`CatalogError::SnapshotUnavailable`].
    ApplyCuration {
        /// Stable source identifier.
        source_id: SourceId,
        /// Source lifecycle generation that must still own the retained database.
        lifecycle_generation: u64,
        /// Exact source revision expected by the curation transaction.
        expected_revision: u64,
        /// Physical source-root identity expected by the curation transaction.
        root_identity: RootIdentity,
        /// Caller-assigned request identifier.
        request_id: u64,
        /// Bounded typed metadata updates.
        updates: Vec<SourceCurationUpdate>,
    },
    /// Apply a bounded sequence of durable, revision-fenced curation batches as one operation.
    ///
    /// Each inner batch is committed through the library's metadata-continuity transaction. The
    /// worker executes batches in order without interleaving another command and emits exactly one
    /// aggregate terminal event containing the committed prefix and, when needed, the failed batch.
    /// An operation remains durable while the worker drains during shutdown.
    ApplyCurationOperation {
        /// Stable source identifier.
        source_id: SourceId,
        /// Source lifecycle generation that must still own the retained database.
        lifecycle_generation: u64,
        /// Exact revision or exact predecessor operation required before the first batch.
        fence: SourceCurationOperationFence,
        /// Physical source-root identity expected by every curation transaction.
        root_identity: RootIdentity,
        /// Monotonic caller-assigned operation identifier for this source lifecycle.
        request_id: u64,
        /// Bounded library-sized curation batches.
        batches: Vec<Vec<SourceCurationUpdate>>,
    },
    /// Read the last coherent persisted snapshot for one source.
    ReadSnapshot {
        /// Stable source identifier.
        source_id: SourceId,
        /// Optional revision fence required by the caller.
        expected_revision: Option<u64>,
    },
    /// Remove one registered source lifecycle generation from the worker projection.
    Remove {
        /// Stable source identifier.
        source_id: SourceId,
        /// Generation that must still own the source.
        lifecycle_generation: u64,
    },
    /// Close the worker after canceling active audits.
    Shutdown,
}

/// Bounded source-tag query accepted by [`CatalogCommand::RequestTagPage`].
#[derive(Debug)]
pub enum CatalogTagPageQuery {
    /// Return tags assigned to this exact set of source-relative paths.
    ///
    /// An empty set is valid and returns an empty page. Each supplied path is validated for safe
    /// source-relative use before the command enters the worker queue.
    SelectedPaths {
        /// Source-relative paths to inspect.
        relative_paths: Vec<PathBuf>,
    },
    /// Return vocabulary tags whose normalized identity starts with this literal prefix.
    VocabularyPrefix {
        /// Literal UTF-8 prefix to match.
        prefix: String,
    },
}

/// Why a lazy tag-page request could not be admitted.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum CatalogTagPageRequestError {
    /// The selected-path query exceeded the bounded path-count admission limit.
    #[error("tag-page request contains {actual} paths; maximum is {max}")]
    TooManyPaths {
        /// Maximum admitted path count.
        max: usize,
        /// Supplied path count.
        actual: usize,
    },
    /// The selected-path query exceeded its aggregate raw path-byte limit.
    #[error("tag-page request paths contain {actual} bytes; maximum is {max}")]
    PathBytesTooLarge {
        /// Maximum aggregate raw path bytes.
        max: usize,
        /// Supplied aggregate raw path bytes.
        actual: usize,
    },
    /// One selected path could not be represented as a safe source-relative path.
    #[error("invalid tag-page path {path}: {reason}")]
    InvalidPath {
        /// Rejected path spelling.
        path: PathBuf,
        /// Validation failure reported by the shared root-relative path type.
        reason: wavecrate_library::sample_sources::reconciliation::RootRelativePathError,
    },
    /// The vocabulary prefix exceeded the bounded UTF-8 byte limit.
    #[error("tag-page prefix is {actual} UTF-8 bytes; maximum is {max}")]
    PrefixTooLong {
        /// Maximum accepted UTF-8 prefix bytes.
        max: usize,
        /// Supplied UTF-8 prefix bytes.
        actual: usize,
    },
    /// The vocabulary prefix contained an embedded NUL.
    #[error("tag-page prefix contains an embedded NUL byte")]
    PrefixContainsNul,
    /// The requested page size was outside the bounded library range.
    #[error("tag-page limit {actual} is outside 1..={max}")]
    InvalidPageLimit {
        /// Maximum accepted page size.
        max: usize,
        /// Supplied page size.
        actual: usize,
    },
}

/// Why a lazy WAV metadata request could not be admitted.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum CatalogWavMetadataRequestError {
    /// No source-relative paths were supplied.
    #[error("WAV metadata request is empty")]
    Empty,
    /// The request exceeded the bounded exact-path admission limit.
    #[error("WAV metadata request contains {actual} paths; maximum is {max}")]
    TooManyPaths {
        /// Maximum admitted path count.
        max: usize,
        /// Supplied path count.
        actual: usize,
    },
    /// One path could not be represented as a safe source-relative path.
    #[error("invalid WAV metadata path {path}: {reason}")]
    InvalidPath {
        /// Rejected path spelling.
        path: PathBuf,
        /// Validation failure reported by the shared root-relative path type.
        reason: wavecrate_library::sample_sources::reconciliation::RootRelativePathError,
    },
}

/// A bounded admission failure from the catalog command queue.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum CatalogSubmitError {
    /// The worker command queue is full.
    #[error("catalog command queue is full")]
    Full,
    /// The catalog worker has closed or is shutting down.
    #[error("catalog worker is closed")]
    Closed,
    /// A lazy WAV metadata request failed input admission.
    #[error("invalid WAV metadata request: {0}")]
    InvalidWavMetadataRequest(#[from] CatalogWavMetadataRequestError),
    /// A lazy tag-page request failed bounded input admission.
    #[error("invalid tag-page request: {0}")]
    InvalidTagPageRequest(#[from] CatalogTagPageRequestError),
    /// A durable curation request failed bounded or lifecycle admission.
    #[error("invalid source curation request: {0}")]
    InvalidCurationRequest(#[from] CatalogCurationRequestError),
}

/// Startup failures for the profile-owned catalog worker.
#[derive(Debug, Error)]
pub enum CatalogStartError {
    /// The supplied profile capability could not be cloned for the worker.
    #[error("could not clone profile ownership for catalog worker: {0}")]
    ProfileOwnership(#[from] ProfileOwnershipError),
    /// The operating system rejected the worker thread.
    #[error("could not start catalog worker: {0}")]
    ThreadSpawn(#[source] std::io::Error),
}

/// A failure emitted for one catalog operation.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum CatalogError {
    /// The profile root or its retained ownership identity failed validation.
    #[error("profile ownership unavailable: {0}")]
    ProfileUnavailable(String),
    /// The source root was not a canonical absolute directory.
    #[error("invalid source root {0}")]
    InvalidRoot(PathBuf),
    /// The source root changed identity while an audit was running.
    #[error("source root generation changed for {source_id}: {detail}")]
    StaleRootGeneration {
        /// Source whose root changed.
        source_id: SourceId,
        /// Stable diagnostic from the scanner or identity check.
        detail: String,
    },
    /// A command named a generation that no longer owns the source.
    #[error("stale lifecycle generation for {source_id}: expected {expected}, received {received}")]
    StaleLifecycleGeneration {
        /// Source whose generation was stale.
        source_id: SourceId,
        /// Current registered generation.
        expected: u64,
        /// Generation supplied by the command.
        received: u64,
    },
    /// The source was not registered.
    #[error("source is not registered: {0}")]
    NotRegistered(SourceId),
    /// The requested snapshot revision was not current.
    #[error("catalog snapshot revision mismatch: expected {expected}, actual {actual}")]
    StaleRevision {
        /// Revision required by the caller.
        expected: u64,
        /// Revision observed in the database.
        actual: u64,
    },
    /// The database has no complete authoritative catalog snapshot.
    #[error("catalog snapshot is unavailable until a complete audit commits")]
    SnapshotUnavailable,
    /// A selected-path tag query named a path outside the retained live manifest.
    #[error("tag-page path is not a live source member for {source_id}: {path}")]
    TagPagePathMissing {
        /// Source whose retained manifest was queried.
        source_id: SourceId,
        /// Missing source-relative path.
        path: PathBuf,
    },
    /// A compound curation operation named a predecessor that is not the latest full success.
    #[error("curation predecessor operation {request_id} is unavailable for {source_id}")]
    CurationPredecessorUnavailable {
        /// Source whose curation lineage was queried.
        source_id: SourceId,
        /// Predecessor operation identifier supplied by the caller.
        request_id: u64,
    },
    /// A source revision could not advance because it reached the integer limit.
    #[error("source revision cannot advance beyond {0}")]
    CurationRevisionOverflow(u64),
    /// The directory-truth projection is not coherent with the manifest revision.
    #[error("catalog directory truth is unavailable or not revision-coherent: {0}")]
    DirectoryTruthUnavailable(String),
    /// A source database operation failed.
    #[error("catalog database operation failed: {0}")]
    Database(String),
    /// The scanner could not complete its authoritative audit.
    #[error("catalog audit failed: {0}")]
    Audit(String),
    /// An audit was canceled before it could publish.
    #[error("catalog audit canceled")]
    Canceled,
}

/// WAV header metadata captured while auditing a catalog manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogWavMetadata {
    /// Number of audio frames in the audited WAV data chunk.
    pub frames: u32,
    /// WAV sample rate in samples per second.
    pub sample_rate: u32,
}

/// A revision-fenced, authoritative source snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogSnapshot {
    /// Stable source identifier.
    pub source_id: SourceId,
    /// Physical source-root identity captured by the committed audit.
    pub root_identity: RootIdentity,
    /// Source lifecycle generation that owns this snapshot.
    pub lifecycle_generation: u64,
    /// Generic source revision shared by the manifest and directory truth.
    pub committed_revision: u64,
    /// Active source-directory generation.
    pub directory_generation: u64,
    /// Manifest rows visible at `committed_revision`.
    pub manifest: Vec<SourceManifestEntry>,
    /// Directory rows visible at `committed_revision`.
    pub directories: Vec<SourceDirectoryEntry>,
    /// Optional WAV header metadata keyed by source-relative manifest path.
    pub wav_metadata: BTreeMap<PathBuf, CatalogWavMetadata>,
    /// Revision-fenced browser curation metadata keyed by source-relative manifest path.
    pub curation_metadata: BTreeMap<PathBuf, BrowserFileMetadata>,
}

/// The operation associated with a failed catalog event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CatalogOperation {
    /// Registration or replacement failed.
    Register,
    /// An authoritative audit failed.
    Audit,
    /// A revision-fenced snapshot read failed.
    ReadSnapshot,
    /// Removal failed.
    Remove,
}

/// Worker output carrying source identity, lifecycle generation, and revision fences.
#[derive(Debug)]
pub enum CatalogEvent {
    /// Registration completed; a persisted snapshot may follow immediately.
    Registered {
        /// Stable source identifier.
        source_id: SourceId,
        /// Registered lifecycle generation.
        lifecycle_generation: u64,
    },
    /// A coherent snapshot was loaded from the catalog database.
    SnapshotReady {
        /// Revision-fenced catalog snapshot.
        snapshot: CatalogSnapshot,
    },
    /// A complete authoritative audit committed and produced a coherent snapshot.
    AuditCommitted {
        /// Revision-fenced catalog snapshot.
        snapshot: CatalogSnapshot,
    },
    /// A bounded exact-path WAV metadata request completed.
    ///
    /// `requested_paths` is the terminal set for this request. Paths absent from `metadata`
    /// were attempted and intentionally omitted by the scanner because they were missing,
    /// malformed, changed, or not WAV files.
    WavMetadataReady {
        /// Stable source identifier.
        source_id: SourceId,
        /// Source lifecycle generation that produced the patch.
        lifecycle_generation: u64,
        /// Physical source-root identity checked by the metadata read.
        root_identity: RootIdentity,
        /// Catalog revision represented by the patch.
        revision: u64,
        /// Caller-assigned request identifier.
        request_id: u64,
        /// Exact paths that reached terminal completion, including omitted paths.
        requested_paths: Vec<PathBuf>,
        /// Successfully parsed WAV metadata keyed by source-relative path.
        metadata: BTreeMap<PathBuf, CatalogWavMetadata>,
    },
    /// A bounded exact-path WAV metadata request failed after admission.
    WavMetadataFailed {
        /// Stable source identifier.
        source_id: SourceId,
        /// Source lifecycle generation associated with the request.
        lifecycle_generation: u64,
        /// Revision required by the request.
        expected_revision: u64,
        /// Caller-assigned request identifier.
        request_id: u64,
        /// Typed failure.
        error: CatalogError,
    },
    /// A bounded source-tag page completed.
    TagPageReady {
        /// Stable source identifier.
        source_id: SourceId,
        /// Source lifecycle generation that produced the page.
        lifecycle_generation: u64,
        /// Physical source-root identity checked by the tag read.
        root_identity: RootIdentity,
        /// Catalog revision represented by the page.
        revision: u64,
        /// Caller-assigned query identifier.
        request_id: u64,
        /// Bounded page of tag rows.
        page: SourceTagPage,
    },
    /// A bounded source-tag page failed after admission.
    TagPageFailed {
        /// Stable source identifier.
        source_id: SourceId,
        /// Source lifecycle generation associated with the request.
        lifecycle_generation: u64,
        /// Revision required by the request.
        expected_revision: u64,
        /// Revision observed before a typed failure, when available.
        observed_revision: Option<u64>,
        /// Caller-assigned query identifier.
        request_id: u64,
        /// Typed failure.
        error: CatalogError,
    },
    /// A durable curation batch committed and returned its bounded metadata patch.
    CurationCommitted {
        /// Stable source identifier.
        source_id: SourceId,
        /// Source lifecycle generation that produced the commit.
        lifecycle_generation: u64,
        /// Caller-assigned request identifier.
        request_id: u64,
        /// Opaque library receipt for the exact committed revision and root.
        receipt: SourceCurationCommitReceipt,
    },
    /// A durable curation batch reached a terminal failure without mutating the source.
    CurationFailed {
        /// Stable source identifier.
        source_id: SourceId,
        /// Source lifecycle generation associated with the request.
        lifecycle_generation: u64,
        /// Exact source revision requested by the command.
        expected_revision: u64,
        /// Caller-assigned request identifier.
        request_id: u64,
        /// Typed failure.
        error: CatalogError,
    },
    /// A bounded compound curation operation reached its one terminal outcome.
    CurationOperationCompleted {
        /// Stable source identifier.
        source_id: SourceId,
        /// Source lifecycle generation associated with the operation.
        lifecycle_generation: u64,
        /// Caller-assigned operation identifier.
        request_id: u64,
        /// Committed prefix and optional failed batch detail.
        outcome: SourceCurationOperationOutcome,
    },
    /// One source was removed from the active worker projection.
    Removed {
        /// Stable source identifier.
        source_id: SourceId,
        /// Removed lifecycle generation.
        lifecycle_generation: u64,
    },
    /// One command failed while the prior in-memory snapshot remained retained.
    Failed {
        /// Source associated with the operation, when known.
        source_id: Option<SourceId>,
        /// Lifecycle generation associated with the operation, when known.
        lifecycle_generation: Option<u64>,
        /// Failed operation.
        operation: CatalogOperation,
        /// Typed failure.
        error: CatalogError,
    },
    /// The worker has canceled active work and closed.
    Closed,
}

struct RegisteredSource {
    root: PathBuf,
    lifecycle_generation: u64,
    database: SourceDatabase,
    known_root_identity: Option<RootIdentity>,
    snapshot: Option<CatalogSnapshot>,
    attempt_token: Option<Arc<AtomicBool>>,
    curation_lineage: Option<CurationLineage>,
}

struct DesiredSource {
    lifecycle_generation: u64,
    active: bool,
    attempt_token: Option<Arc<AtomicBool>>,
    last_curation_request_id: Option<u64>,
}

#[derive(Clone, Copy)]
struct CurationLineage {
    last_terminal_request_id: u64,
    last_success_revision: Option<u64>,
}

struct AdmissionState {
    shutdown: bool,
    closed: bool,
    event_budget: usize,
    curation_payload_bytes: usize,
    curation_update_count: usize,
    tag_page_input_bytes: usize,
    sources: HashMap<SourceId, DesiredSource>,
}

impl AdmissionState {
    fn new() -> Self {
        Self {
            shutdown: false,
            closed: false,
            event_budget: 1,
            curation_payload_bytes: 0,
            curation_update_count: 0,
            tag_page_input_bytes: 0,
            sources: HashMap::new(),
        }
    }

    fn invalidate_all(&mut self) {
        for source in self.sources.values_mut() {
            if let Some(token) = source.attempt_token.take() {
                token.store(true, Ordering::Release);
            }
            source.active = false;
        }
    }
}

struct EventQueue {
    events: VecDeque<QueuedEvent>,
    closed: bool,
}

struct QueuedEvent {
    event: CatalogEvent,
    curation_payload_bytes: usize,
    curation_update_count: usize,
}

struct CommandInputReservation<'a> {
    admission: &'a Arc<Mutex<AdmissionState>>,
    bytes: usize,
}

impl Drop for CommandInputReservation<'_> {
    fn drop(&mut self) {
        if self.bytes == 0 {
            return;
        }
        let mut admission = self
            .admission
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        admission.tag_page_input_bytes = admission.tag_page_input_bytes.saturating_sub(self.bytes);
    }
}

struct EventHub {
    queue: Mutex<EventQueue>,
    wake: Condvar,
}

impl EventHub {
    fn new() -> Self {
        Self {
            queue: Mutex::new(EventQueue {
                events: VecDeque::new(),
                closed: false,
            }),
            wake: Condvar::new(),
        }
    }

    fn send(
        &self,
        event: CatalogEvent,
        curation_payload_bytes: usize,
        curation_update_count: usize,
    ) -> bool {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if queue.events.len() >= EVENT_CAPACITY {
            return false;
        }
        queue.events.push_back(QueuedEvent {
            event,
            curation_payload_bytes,
            curation_update_count,
        });
        self.wake.notify_one();
        true
    }

    fn close(&self) {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        queue.closed = true;
        self.wake.notify_all();
    }

    fn try_recv(&self) -> Result<(CatalogEvent, usize, usize), TryRecvError> {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(event) = queue.events.pop_front() {
            return Ok((
                event.event,
                event.curation_payload_bytes,
                event.curation_update_count,
            ));
        }
        if queue.closed {
            Err(TryRecvError::Disconnected)
        } else {
            Err(TryRecvError::Empty)
        }
    }

    fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<(CatalogEvent, usize, usize), mpsc::RecvTimeoutError> {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(event) = queue.events.pop_front() {
            return Ok((
                event.event,
                event.curation_payload_bytes,
                event.curation_update_count,
            ));
        }
        if queue.closed {
            return Err(mpsc::RecvTimeoutError::Disconnected);
        }
        let (mut queue, wait) = self
            .wake
            .wait_timeout(queue, timeout)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(event) = queue.events.pop_front() {
            return Ok((
                event.event,
                event.curation_payload_bytes,
                event.curation_update_count,
            ));
        }
        if queue.closed {
            Err(mpsc::RecvTimeoutError::Disconnected)
        } else if wait.timed_out() {
            Err(mpsc::RecvTimeoutError::Timeout)
        } else {
            // A spurious wakeup is reported as a bounded timeout; callers already poll again.
            Err(mpsc::RecvTimeoutError::Timeout)
        }
    }
}

struct EventReservation<'a> {
    hub: &'a EventHub,
    admission: &'a Arc<Mutex<AdmissionState>>,
    remaining: usize,
    curation_payload_bytes: usize,
    curation_update_count: usize,
}

impl EventReservation<'_> {
    fn publish(&mut self, event: CatalogEvent) {
        assert!(
            self.hub.send(
                event,
                self.curation_payload_bytes,
                self.curation_update_count,
            ),
            "catalog event capacity accounting invariant violated"
        );
        self.remaining = self.remaining.saturating_sub(1);
        self.curation_payload_bytes = 0;
        self.curation_update_count = 0;
    }

    fn finish(self) {
        if self.remaining == 0
            && self.curation_payload_bytes == 0
            && self.curation_update_count == 0
        {
            return;
        }
        let mut state = self
            .admission
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.event_budget = state.event_budget.saturating_sub(self.remaining);
        state.curation_payload_bytes = state
            .curation_payload_bytes
            .saturating_sub(self.curation_payload_bytes);
        state.curation_update_count = state
            .curation_update_count
            .saturating_sub(self.curation_update_count);
    }
}

fn validate_command(command: &CatalogCommand) -> Result<(), CatalogSubmitError> {
    match command {
        CatalogCommand::RequestWavMetadata { relative_paths, .. } => {
            if relative_paths.is_empty() {
                return Err(CatalogSubmitError::InvalidWavMetadataRequest(
                    CatalogWavMetadataRequestError::Empty,
                ));
            }
            if relative_paths.len() > MAX_WAV_METADATA_REQUEST_PATHS {
                return Err(CatalogSubmitError::InvalidWavMetadataRequest(
                    CatalogWavMetadataRequestError::TooManyPaths {
                        max: MAX_WAV_METADATA_REQUEST_PATHS,
                        actual: relative_paths.len(),
                    },
                ));
            }
            for path in relative_paths {
                if let Err(reason) = RootRelativePath::try_from_path(path.clone()) {
                    return Err(CatalogSubmitError::InvalidWavMetadataRequest(
                        CatalogWavMetadataRequestError::InvalidPath {
                            path: path.clone(),
                            reason,
                        },
                    ));
                }
            }
        }
        CatalogCommand::RequestTagPage {
            query, page_limit, ..
        } => validate_tag_page_request(query, *page_limit)?,
        _ => {}
    }
    Ok(())
}

fn validate_tag_page_request(
    query: &CatalogTagPageQuery,
    page_limit: usize,
) -> Result<(), CatalogSubmitError> {
    if !(1..=SOURCE_TAG_PAGE_MAX_PAGE_SIZE).contains(&page_limit) {
        return Err(CatalogSubmitError::InvalidTagPageRequest(
            CatalogTagPageRequestError::InvalidPageLimit {
                max: SOURCE_TAG_PAGE_MAX_PAGE_SIZE,
                actual: page_limit,
            },
        ));
    }
    match query {
        CatalogTagPageQuery::SelectedPaths { relative_paths } => {
            if relative_paths.len() > SOURCE_TAG_PAGE_MAX_PATHS {
                return Err(CatalogSubmitError::InvalidTagPageRequest(
                    CatalogTagPageRequestError::TooManyPaths {
                        max: SOURCE_TAG_PAGE_MAX_PATHS,
                        actual: relative_paths.len(),
                    },
                ));
            }
            let path_bytes = relative_paths.iter().fold(0usize, |total, path| {
                total.saturating_add(encoded_path_len(path))
            });
            if path_bytes > SOURCE_TAG_PAGE_MAX_PATH_BYTES {
                return Err(CatalogSubmitError::InvalidTagPageRequest(
                    CatalogTagPageRequestError::PathBytesTooLarge {
                        max: SOURCE_TAG_PAGE_MAX_PATH_BYTES,
                        actual: path_bytes,
                    },
                ));
            }
            for path in relative_paths {
                if let Err(reason) = RootRelativePath::try_from_path(path.clone()) {
                    return Err(CatalogSubmitError::InvalidTagPageRequest(
                        CatalogTagPageRequestError::InvalidPath {
                            path: path.clone(),
                            reason,
                        },
                    ));
                }
            }
        }
        CatalogTagPageQuery::VocabularyPrefix { prefix } => {
            if prefix.len() > SOURCE_TAG_PAGE_MAX_PREFIX_BYTES {
                return Err(CatalogSubmitError::InvalidTagPageRequest(
                    CatalogTagPageRequestError::PrefixTooLong {
                        max: SOURCE_TAG_PAGE_MAX_PREFIX_BYTES,
                        actual: prefix.len(),
                    },
                ));
            }
            if prefix.contains('\0') {
                return Err(CatalogSubmitError::InvalidTagPageRequest(
                    CatalogTagPageRequestError::PrefixContainsNul,
                ));
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn encoded_path_len(path: &Path) -> usize {
    use std::os::unix::ffi::OsStrExt;

    path.as_os_str().as_bytes().len()
}

#[cfg(not(unix))]
fn encoded_path_len(path: &Path) -> usize {
    path.to_string_lossy().len()
}

fn command_event_budget(command: &CatalogCommand) -> usize {
    match command {
        CatalogCommand::Register { .. } => 2,
        CatalogCommand::RequestAudit { .. }
        | CatalogCommand::RequestWavMetadata { .. }
        | CatalogCommand::RequestTagPage { .. }
        | CatalogCommand::ApplyCuration { .. }
        | CatalogCommand::ApplyCurationOperation { .. }
        | CatalogCommand::ReadSnapshot { .. }
        | CatalogCommand::Remove { .. }
        | CatalogCommand::Shutdown => 1,
    }
}

fn command_curation_payload_bytes(command: &CatalogCommand) -> usize {
    match command {
        CatalogCommand::ApplyCuration {
            root_identity,
            updates,
            ..
        } => curation::operation_payload_bytes(root_identity, std::slice::from_ref(updates)),
        CatalogCommand::ApplyCurationOperation {
            root_identity,
            batches,
            ..
        } => curation::operation_payload_bytes(root_identity, batches),
        _ => 0,
    }
}

fn command_curation_update_count(command: &CatalogCommand) -> usize {
    match command {
        CatalogCommand::ApplyCurationOperation { batches, .. } => {
            batches.iter().map(Vec::len).sum()
        }
        _ => 0,
    }
}

fn command_tag_page_input_bytes(command: &CatalogCommand) -> usize {
    let CatalogCommand::RequestTagPage { query, .. } = command else {
        return 0;
    };
    let CatalogTagPageQuery::SelectedPaths { relative_paths } = query else {
        return 0;
    };
    selected_paths_input_bytes(relative_paths)
}

fn selected_paths_input_bytes(relative_paths: &[PathBuf]) -> usize {
    let raw_bytes = relative_paths.iter().fold(0usize, |total, path| {
        total.saturating_add(encoded_path_len(path))
    });
    raw_bytes.saturating_add(
        relative_paths
            .len()
            .saturating_mul(std::mem::size_of::<PathBuf>()),
    )
}

#[derive(Clone)]
enum AdmissionAction {
    Register {
        source_id: SourceId,
        lifecycle_generation: u64,
    },
    RequestAudit,
    RequestWavMetadata,
    RequestTagPage,
    ApplyCuration,
    ApplyCurationOperation {
        source_id: SourceId,
        request_id: u64,
    },
    ReadSnapshot,
    Remove {
        source_id: SourceId,
        lifecycle_generation: u64,
    },
    Shutdown,
}

fn admission_action(command: &CatalogCommand) -> AdmissionAction {
    match command {
        CatalogCommand::Register {
            source_id,
            lifecycle_generation,
            ..
        } => AdmissionAction::Register {
            source_id: source_id.clone(),
            lifecycle_generation: *lifecycle_generation,
        },
        CatalogCommand::RequestAudit { .. } => AdmissionAction::RequestAudit,
        CatalogCommand::RequestWavMetadata { .. } => AdmissionAction::RequestWavMetadata,
        CatalogCommand::RequestTagPage { .. } => AdmissionAction::RequestTagPage,
        CatalogCommand::ApplyCuration { .. } => AdmissionAction::ApplyCuration,
        CatalogCommand::ApplyCurationOperation {
            source_id,
            request_id,
            ..
        } => AdmissionAction::ApplyCurationOperation {
            source_id: source_id.clone(),
            request_id: *request_id,
        },
        CatalogCommand::ReadSnapshot { .. } => AdmissionAction::ReadSnapshot,
        CatalogCommand::Remove {
            source_id,
            lifecycle_generation,
        } => AdmissionAction::Remove {
            source_id: source_id.clone(),
            lifecycle_generation: *lifecycle_generation,
        },
        CatalogCommand::Shutdown => AdmissionAction::Shutdown,
    }
}

fn apply_admission_action(state: &mut AdmissionState, action: AdmissionAction) {
    match action {
        AdmissionAction::Register {
            source_id,
            lifecycle_generation,
        } => {
            let replace = state
                .sources
                .get(&source_id)
                .is_none_or(|source| lifecycle_generation > source.lifecycle_generation);
            if replace {
                if let Some(previous) = state.sources.get_mut(&source_id)
                    && let Some(token) = previous.attempt_token.take()
                {
                    token.store(true, Ordering::Release);
                }
                state.sources.insert(
                    source_id,
                    DesiredSource {
                        lifecycle_generation,
                        active: true,
                        attempt_token: None,
                        last_curation_request_id: None,
                    },
                );
            }
        }
        AdmissionAction::Remove {
            source_id,
            lifecycle_generation,
        } => {
            let replace = state
                .sources
                .get(&source_id)
                .is_none_or(|source| lifecycle_generation >= source.lifecycle_generation);
            if replace {
                if let Some(previous) = state.sources.get_mut(&source_id)
                    && let Some(token) = previous.attempt_token.take()
                {
                    token.store(true, Ordering::Release);
                }
                state.sources.insert(
                    source_id,
                    DesiredSource {
                        lifecycle_generation,
                        active: false,
                        attempt_token: None,
                        last_curation_request_id: None,
                    },
                );
            }
        }
        AdmissionAction::Shutdown => {
            state.shutdown = true;
            state.invalidate_all();
        }
        AdmissionAction::RequestAudit
        | AdmissionAction::RequestWavMetadata
        | AdmissionAction::RequestTagPage
        | AdmissionAction::ApplyCuration
        | AdmissionAction::ReadSnapshot => {}
        AdmissionAction::ApplyCurationOperation {
            source_id,
            request_id,
        } => {
            if let Some(source) = state.sources.get_mut(&source_id) {
                source.last_curation_request_id = Some(request_id);
            }
        }
    }
}

fn admission_state<T>(
    admission: &Arc<Mutex<AdmissionState>>,
    read: impl FnOnce(&AdmissionState) -> T,
) -> T {
    let state = admission
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    read(&state)
}

fn begin_source_attempt(
    admission: &Arc<Mutex<AdmissionState>>,
    source_id: &SourceId,
    lifecycle_generation: u64,
) -> Option<Arc<AtomicBool>> {
    let mut state = admission
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if state.shutdown || state.closed {
        return None;
    }
    let source = state.sources.get_mut(source_id)?;
    if !source.active || source.lifecycle_generation != lifecycle_generation {
        return None;
    }
    if let Some(previous) = source.attempt_token.take() {
        previous.store(true, Ordering::Release);
    }
    let token = Arc::new(AtomicBool::new(false));
    source.attempt_token = Some(Arc::clone(&token));
    Some(token)
}

fn publication_is_current(
    admission: &Arc<Mutex<AdmissionState>>,
    source_id: &SourceId,
    lifecycle_generation: u64,
    token: Option<&Arc<AtomicBool>>,
) -> bool {
    admission_state(admission, |state| {
        publication_is_current_state(state, source_id, lifecycle_generation, token)
    })
}

fn publication_is_current_state(
    state: &AdmissionState,
    source_id: &SourceId,
    lifecycle_generation: u64,
    token: Option<&Arc<AtomicBool>>,
) -> bool {
    if state.shutdown || state.closed {
        return false;
    }
    let Some(source) = state.sources.get(source_id) else {
        return false;
    };
    if !source.active || source.lifecycle_generation != lifecycle_generation {
        return false;
    }
    match token {
        Some(expected) => source.attempt_token.as_ref().is_some_and(|current| {
            Arc::ptr_eq(expected, current) && !expected.load(Ordering::Acquire)
        }),
        None => true,
    }
}

fn publish_if_current(
    admission: &Arc<Mutex<AdmissionState>>,
    reservation: &mut EventReservation<'_>,
    source_id: &SourceId,
    lifecycle_generation: u64,
    token: Option<&Arc<AtomicBool>>,
    event: CatalogEvent,
) -> bool {
    let state = admission
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !publication_is_current_state(&state, source_id, lifecycle_generation, token) {
        return false;
    }
    reservation.publish(event);
    true
}

fn publish_removal_if_current(
    admission: &Arc<Mutex<AdmissionState>>,
    reservation: &mut EventReservation<'_>,
    source_id: &SourceId,
    lifecycle_generation: u64,
) -> bool {
    let state = admission
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let current = !state.shutdown
        && !state.closed
        && state.sources.get(source_id).is_some_and(|source| {
            !source.active && source.lifecycle_generation == lifecycle_generation
        });
    if !current {
        return false;
    }
    reservation.publish(CatalogEvent::Removed {
        source_id: source_id.clone(),
        lifecycle_generation,
    });
    true
}

/// Profile-owned catalog worker handle.
pub struct CatalogService {
    commands: SyncSender<CatalogCommand>,
    events: Arc<EventHub>,
    admission: Arc<Mutex<AdmissionState>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl CatalogService {
    /// Start a catalog worker using a clone of an already-acquired profile capability.
    pub fn start(profile_guard: &WritableProfileGuard) -> Result<Self, CatalogStartError> {
        Self::start_inner(profile_guard, None)
    }

    #[cfg(test)]
    fn start_with_test_gate(
        profile_guard: &WritableProfileGuard,
    ) -> Result<(Self, Arc<CatalogTestGate>), CatalogStartError> {
        let test_gate = Arc::new(CatalogTestGate::new());
        let service = Self::start_inner(profile_guard, Some(Arc::clone(&test_gate)))?;
        Ok((service, test_gate))
    }

    fn start_inner(
        profile_guard: &WritableProfileGuard,
        test_gate: Option<Arc<CatalogTestGate>>,
    ) -> Result<Self, CatalogStartError> {
        let worker_profile_guard = profile_guard.try_clone()?;
        let (commands, command_rx) = mpsc::sync_channel(COMMAND_CAPACITY);
        let events = Arc::new(EventHub::new());
        let worker_events = Arc::clone(&events);
        let admission = Arc::new(Mutex::new(AdmissionState::new()));
        let worker_admission = Arc::clone(&admission);
        let worker_test_gate = test_gate;
        let worker = thread::Builder::new()
            .name(String::from("wavecrate-catalog"))
            .spawn(move || {
                run_worker(
                    worker_profile_guard,
                    command_rx,
                    worker_events,
                    worker_admission,
                    worker_test_gate,
                )
            })
            .map_err(CatalogStartError::ThreadSpawn)?;

        Ok(Self {
            commands,
            events,
            admission,
            worker: Mutex::new(Some(worker)),
        })
    }

    /// Submit one typed operation without waiting for the worker.
    pub fn submit(&self, command: CatalogCommand) -> Result<(), CatalogSubmitError> {
        let action = admission_action(&command);
        let mut admission = self
            .admission
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if admission.closed || admission.shutdown {
            return Err(CatalogSubmitError::Closed);
        }
        validate_command(&command)?;
        curation::validate_submission(&command, &admission)
            .map_err(CatalogSubmitError::InvalidCurationRequest)?;
        let event_budget = command_event_budget(&command);
        if admission.event_budget.saturating_add(event_budget) > EVENT_CAPACITY {
            return Err(CatalogSubmitError::Full);
        }
        let curation_payload_bytes = command_curation_payload_bytes(&command);
        let curation_update_count = command_curation_update_count(&command);
        let tag_page_input_bytes = command_tag_page_input_bytes(&command);
        if admission
            .tag_page_input_bytes
            .saturating_add(tag_page_input_bytes)
            > MAX_TAG_PAGE_INPUT_BYTES
        {
            return Err(CatalogSubmitError::Full);
        }
        if admission
            .curation_update_count
            .saturating_add(curation_update_count)
            > MAX_CURATION_OPERATION_UPDATES
        {
            return Err(CatalogSubmitError::Full);
        }
        if admission
            .curation_payload_bytes
            .saturating_add(curation_payload_bytes)
            > MAX_CURATION_OPERATION_RECEIPT_BYTES
        {
            return Err(CatalogSubmitError::Full);
        }
        admission.tag_page_input_bytes = admission
            .tag_page_input_bytes
            .saturating_add(tag_page_input_bytes);
        match self.commands.try_send(command) {
            Ok(()) => {
                admission.event_budget += event_budget;
                admission.curation_payload_bytes += curation_payload_bytes;
                admission.curation_update_count += curation_update_count;
                apply_admission_action(&mut admission, action);
                Ok(())
            }
            Err(TrySendError::Full(_)) => {
                admission.tag_page_input_bytes = admission
                    .tag_page_input_bytes
                    .saturating_sub(tag_page_input_bytes);
                Err(CatalogSubmitError::Full)
            }
            Err(TrySendError::Disconnected(_)) => {
                admission.tag_page_input_bytes = admission
                    .tag_page_input_bytes
                    .saturating_sub(tag_page_input_bytes);
                admission.closed = true;
                Err(CatalogSubmitError::Closed)
            }
        }
    }

    /// Read the next worker event without blocking.
    pub fn try_recv_event(&self) -> Result<CatalogEvent, TryRecvError> {
        let (event, curation_payload_bytes, curation_update_count) = self.events.try_recv()?;
        self.release_event_credit(curation_payload_bytes, curation_update_count);
        Ok(event)
    }

    /// Read the next worker event with a bounded wait.
    pub fn recv_event_timeout(
        &self,
        timeout: Duration,
    ) -> Result<CatalogEvent, mpsc::RecvTimeoutError> {
        let (event, curation_payload_bytes, curation_update_count) =
            self.events.recv_timeout(timeout)?;
        self.release_event_credit(curation_payload_bytes, curation_update_count);
        Ok(event)
    }

    fn release_event_credit(&self, curation_payload_bytes: usize, curation_update_count: usize) {
        let mut admission = self
            .admission
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        admission.event_budget = admission.event_budget.saturating_sub(1);
        admission.curation_payload_bytes = admission
            .curation_payload_bytes
            .saturating_sub(curation_payload_bytes);
        admission.curation_update_count = admission
            .curation_update_count
            .saturating_sub(curation_update_count);
    }

    /// Request cancellation and synchronously join the worker.
    pub fn shutdown(&self) {
        {
            let mut admission = self
                .admission
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !admission.shutdown && !admission.closed {
                admission.shutdown = true;
                admission.invalidate_all();
            }
        }
        if let Ok(mut worker) = self.worker.lock()
            && let Some(worker) = worker.take()
        {
            let _ = worker.join();
        }
    }
}

impl Drop for CatalogService {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run_worker(
    profile_guard: WritableProfileGuard,
    command_rx: Receiver<CatalogCommand>,
    events: Arc<EventHub>,
    admission: Arc<Mutex<AdmissionState>>,
    test_gate: Option<Arc<CatalogTestGate>>,
) {
    let mut sources = HashMap::new();
    loop {
        if let Some(test_gate) = test_gate.as_ref() {
            test_gate.before_receive();
        }
        let closing = admission_state(&admission, |state| state.shutdown || state.closed);
        let command = if closing {
            match command_rx.try_recv() {
                Ok(command) => Some(command),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
            }
        } else {
            match command_rx.recv_timeout(WORKER_IDLE_POLL) {
                Ok(command) => Some(command),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        };
        let Some(command) = command else {
            break;
        };
        let _tag_page_input_reservation = CommandInputReservation {
            admission: &admission,
            bytes: command_tag_page_input_bytes(&command),
        };
        let closing = admission_state(&admission, |state| state.shutdown || state.closed);
        let reserved_events = command_event_budget(&command);
        let mut reservation = EventReservation {
            hub: &events,
            admission: &admission,
            remaining: reserved_events,
            curation_payload_bytes: command_curation_payload_bytes(&command),
            curation_update_count: command_curation_update_count(&command),
        };
        if closing && !curation::is_durable(&command) {
            reservation.finish();
            continue;
        }
        if handle_command(
            command,
            &profile_guard,
            &mut reservation,
            &admission,
            &mut sources,
        ) {
            reservation.finish();
            break;
        }
        reservation.finish();
    }
    {
        let mut state = admission
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.invalidate_all();
        state.closed = true;
    }
    assert!(
        events.send(CatalogEvent::Closed, 0, 0),
        "catalog event capacity accounting invariant violated while closing"
    );
    events.close();
}

fn handle_command(
    command: CatalogCommand,
    profile_guard: &WritableProfileGuard,
    reservation: &mut EventReservation<'_>,
    admission: &Arc<Mutex<AdmissionState>>,
    sources: &mut HashMap<SourceId, RegisteredSource>,
) -> bool {
    match command {
        CatalogCommand::Register {
            source_id,
            canonical_root,
            lifecycle_generation,
        } => {
            register_source(
                source_id,
                canonical_root,
                lifecycle_generation,
                profile_guard,
                reservation,
                admission,
                sources,
            );
            false
        }
        CatalogCommand::RequestAudit {
            source_id,
            lifecycle_generation,
        } => {
            request_audit(
                source_id,
                lifecycle_generation,
                profile_guard,
                reservation,
                admission,
                sources,
            );
            false
        }
        CatalogCommand::RequestWavMetadata {
            source_id,
            lifecycle_generation,
            expected_revision,
            request_id,
            relative_paths,
        } => {
            request_wav_metadata(
                source_id,
                lifecycle_generation,
                expected_revision,
                request_id,
                relative_paths,
                profile_guard,
                reservation,
                admission,
                sources,
            );
            false
        }
        CatalogCommand::RequestTagPage {
            source_id,
            lifecycle_generation,
            expected_revision,
            request_id,
            query,
            after_cursor,
            page_limit,
        } => {
            request_tag_page(
                source_id,
                lifecycle_generation,
                expected_revision,
                request_id,
                query,
                after_cursor,
                page_limit,
                profile_guard,
                reservation,
                admission,
                sources,
            );
            false
        }
        CatalogCommand::ApplyCuration {
            source_id,
            lifecycle_generation,
            expected_revision,
            root_identity,
            request_id,
            updates,
        } => {
            curation::execute(
                source_id,
                lifecycle_generation,
                expected_revision,
                root_identity,
                request_id,
                updates,
                profile_guard,
                reservation,
                sources,
            );
            false
        }
        CatalogCommand::ApplyCurationOperation {
            source_id,
            lifecycle_generation,
            fence,
            root_identity,
            request_id,
            batches,
        } => {
            curation::execute_operation(
                source_id,
                lifecycle_generation,
                fence,
                root_identity,
                request_id,
                batches,
                profile_guard,
                reservation,
                sources,
            );
            false
        }
        CatalogCommand::ReadSnapshot {
            source_id,
            expected_revision,
        } => {
            read_snapshot(
                source_id,
                expected_revision,
                profile_guard,
                reservation,
                admission,
                sources,
            );
            false
        }
        CatalogCommand::Remove {
            source_id,
            lifecycle_generation,
        } => {
            remove_source(
                source_id,
                lifecycle_generation,
                reservation,
                admission,
                sources,
            );
            false
        }
        CatalogCommand::Shutdown => {
            let mut state = admission
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.shutdown = true;
            state.invalidate_all();
            true
        }
    }
}

fn request_tag_page(
    source_id: SourceId,
    lifecycle_generation: u64,
    expected_revision: u64,
    request_id: u64,
    query: CatalogTagPageQuery,
    after_cursor: Option<SourceTagPageCursor>,
    page_limit: usize,
    profile_guard: &WritableProfileGuard,
    reservation: &mut EventReservation<'_>,
    admission: &Arc<Mutex<AdmissionState>>,
    sources: &mut HashMap<SourceId, RegisteredSource>,
) {
    let Some(current_generation) = sources
        .get(&source_id)
        .map(|source| source.lifecycle_generation)
    else {
        let error = CatalogError::NotRegistered(source_id.clone());
        emit_tag_page_failure(
            reservation,
            admission,
            source_id,
            lifecycle_generation,
            expected_revision,
            request_id,
            None,
            error,
        );
        return;
    };
    if let Err(error) = ensure_generation(&source_id, current_generation, lifecycle_generation) {
        emit_tag_page_failure(
            reservation,
            admission,
            source_id,
            lifecycle_generation,
            expected_revision,
            request_id,
            None,
            error,
        );
        return;
    }
    let Some(token) = begin_source_attempt(admission, &source_id, lifecycle_generation) else {
        return;
    };
    let Some(source) = sources.get_mut(&source_id) else {
        return;
    };
    source.attempt_token = Some(Arc::clone(&token));
    let result = tags::read_page(
        source,
        &source_id,
        lifecycle_generation,
        expected_revision,
        &query,
        after_cursor.as_ref(),
        page_limit,
        &token,
        admission,
        profile_guard,
    );
    match result {
        Ok(patch) => {
            let _ = publish_if_current(
                admission,
                reservation,
                &source_id,
                lifecycle_generation,
                Some(&token),
                CatalogEvent::TagPageReady {
                    source_id: source_id.clone(),
                    lifecycle_generation,
                    root_identity: patch.root_identity,
                    revision: patch.revision,
                    request_id,
                    page: patch.page,
                },
            );
        }
        Err(error) => emit_tag_page_failure(
            reservation,
            admission,
            source_id,
            lifecycle_generation,
            expected_revision,
            request_id,
            Some(&token),
            error,
        ),
    }
}

fn register_source(
    source_id: SourceId,
    canonical_root: PathBuf,
    lifecycle_generation: u64,
    profile_guard: &WritableProfileGuard,
    reservation: &mut EventReservation<'_>,
    admission: &Arc<Mutex<AdmissionState>>,
    sources: &mut HashMap<SourceId, RegisteredSource>,
) {
    if let Err(error) = validate_canonical_root(&canonical_root) {
        emit_failure(
            reservation,
            admission,
            Some(source_id),
            Some(lifecycle_generation),
            CatalogOperation::Register,
            error,
        );
        return;
    }
    if !publication_is_current(admission, &source_id, lifecycle_generation, None) {
        return;
    }
    if let Some(current) = sources.get(&source_id)
        && lifecycle_generation <= current.lifecycle_generation
    {
        emit_failure(
            reservation,
            admission,
            Some(source_id.clone()),
            Some(lifecycle_generation),
            CatalogOperation::Register,
            CatalogError::StaleLifecycleGeneration {
                source_id,
                expected: current.lifecycle_generation,
                received: lifecycle_generation,
            },
        );
        return;
    }
    // The profile child lease is retained by the previous database connection. Release that
    // connection before opening the replacement generation; the admission token has already
    // invalidated any audit that used the old projection.
    sources.remove(&source_id);
    if let Err(error) = profile_guard.validate_current() {
        emit_failure(
            reservation,
            admission,
            Some(source_id),
            Some(lifecycle_generation),
            CatalogOperation::Register,
            CatalogError::ProfileUnavailable(error.to_string()),
        );
        return;
    }
    let profile_child = catalog_profile_child(&source_id);
    let database = match SourceDatabase::open_for_catalog_with_profile_guard(
        &canonical_root,
        profile_guard,
        &profile_child,
    ) {
        Ok(database) => database,
        Err(error) => {
            emit_failure(
                reservation,
                admission,
                Some(source_id),
                Some(lifecycle_generation),
                CatalogOperation::Register,
                catalog_error_from_db(error),
            );
            return;
        }
    };
    if let Err(error) = database.validate_profile_guard(profile_guard) {
        emit_failure(
            reservation,
            admission,
            Some(source_id),
            Some(lifecycle_generation),
            CatalogOperation::Register,
            CatalogError::ProfileUnavailable(error.to_string()),
        );
        return;
    }
    if !publication_is_current(admission, &source_id, lifecycle_generation, None) {
        return;
    }
    let mut source = RegisteredSource {
        root: canonical_root,
        lifecycle_generation,
        database,
        known_root_identity: None,
        snapshot: None,
        attempt_token: None,
        curation_lineage: None,
    };
    if let Err(error) = maintain_catalog_database(&source.database, profile_guard, None, || {
        !publication_is_current(admission, &source_id, lifecycle_generation, None)
    }) {
        emit_failure(
            reservation,
            admission,
            Some(source_id),
            Some(lifecycle_generation),
            CatalogOperation::Register,
            error,
        );
        return;
    }
    if let Ok(identity) = source_root_identity(&source.root)
        && let Ok(mut snapshot) = read_snapshot_from_database(
            &source.database,
            profile_guard,
            &source_id,
            lifecycle_generation,
            &identity,
            || !publication_is_current(admission, &source_id, lifecycle_generation, None),
        )
        && attach_wav_metadata(
            &mut snapshot,
            &source.root,
            &source.database,
            profile_guard,
            &source_id,
            &identity,
            || !publication_is_current(admission, &source_id, lifecycle_generation, None),
        )
        .is_ok()
    {
        source.known_root_identity = Some(identity);
        source.snapshot = Some(snapshot);
    }
    sources.insert(source_id.clone(), source);
    if !publication_is_current(admission, &source_id, lifecycle_generation, None) {
        sources.remove(&source_id);
        return;
    }
    let _ = publish_if_current(
        admission,
        reservation,
        &source_id,
        lifecycle_generation,
        None,
        CatalogEvent::Registered {
            source_id: source_id.clone(),
            lifecycle_generation,
        },
    );
    if let Some(snapshot) = sources
        .get(&source_id)
        .and_then(|source| source.snapshot.clone())
    {
        let _ = publish_if_current(
            admission,
            reservation,
            &source_id,
            lifecycle_generation,
            None,
            CatalogEvent::SnapshotReady { snapshot },
        );
    }
}

fn request_audit(
    source_id: SourceId,
    lifecycle_generation: u64,
    profile_guard: &WritableProfileGuard,
    reservation: &mut EventReservation<'_>,
    admission: &Arc<Mutex<AdmissionState>>,
    sources: &mut HashMap<SourceId, RegisteredSource>,
) {
    let Some(current_generation) = sources
        .get(&source_id)
        .map(|source| source.lifecycle_generation)
    else {
        emit_failure(
            reservation,
            admission,
            Some(source_id.clone()),
            Some(lifecycle_generation),
            CatalogOperation::Audit,
            CatalogError::NotRegistered(source_id),
        );
        return;
    };
    if let Err(error) = ensure_generation(&source_id, current_generation, lifecycle_generation) {
        emit_failure(
            reservation,
            admission,
            Some(source_id),
            Some(lifecycle_generation),
            CatalogOperation::Audit,
            error,
        );
        return;
    }
    if let Some(source) = sources.get_mut(&source_id) {
        // An audit is an independent revision authority. A later compound command must not
        // implicitly chain across it, even if this audit ultimately gets canceled or fails.
        source.curation_lineage = None;
    }
    let Some(token) = begin_source_attempt(admission, &source_id, lifecycle_generation) else {
        return;
    };
    let Some(source) = sources.get_mut(&source_id) else {
        return;
    };
    source.attempt_token = Some(Arc::clone(&token));
    let result = maintain_catalog_database(&source.database, profile_guard, Some(&token), || {
        token.load(Ordering::Acquire)
    })
    .and_then(|()| audit_source(source, profile_guard, &source_id, &token));
    match result {
        Ok(snapshot) => {
            source.known_root_identity = Some(snapshot.root_identity.clone());
            source.snapshot = Some(snapshot.clone());
            let _ = publish_if_current(
                admission,
                reservation,
                &source_id,
                lifecycle_generation,
                Some(&token),
                CatalogEvent::AuditCommitted { snapshot },
            );
        }
        Err(error) => {
            let _ = publish_if_current(
                admission,
                reservation,
                &source_id,
                lifecycle_generation,
                Some(&token),
                CatalogEvent::Failed {
                    source_id: Some(source_id.clone()),
                    lifecycle_generation: Some(lifecycle_generation),
                    operation: CatalogOperation::Audit,
                    error,
                },
            );
        }
    }
}

fn request_wav_metadata(
    source_id: SourceId,
    lifecycle_generation: u64,
    expected_revision: u64,
    request_id: u64,
    requested_paths: Vec<PathBuf>,
    profile_guard: &WritableProfileGuard,
    reservation: &mut EventReservation<'_>,
    admission: &Arc<Mutex<AdmissionState>>,
    sources: &mut HashMap<SourceId, RegisteredSource>,
) {
    let Some(current_generation) = sources
        .get(&source_id)
        .map(|source| source.lifecycle_generation)
    else {
        let error = CatalogError::NotRegistered(source_id.clone());
        emit_metadata_failure(
            reservation,
            admission,
            source_id,
            lifecycle_generation,
            expected_revision,
            request_id,
            error,
        );
        return;
    };
    if let Err(error) = ensure_generation(&source_id, current_generation, lifecycle_generation) {
        emit_metadata_failure(
            reservation,
            admission,
            source_id,
            lifecycle_generation,
            expected_revision,
            request_id,
            error,
        );
        return;
    }
    let Some(token) = begin_source_attempt(admission, &source_id, lifecycle_generation) else {
        return;
    };
    let Some(source) = sources.get_mut(&source_id) else {
        return;
    };
    source.attempt_token = Some(Arc::clone(&token));
    let result = wav_metadata::read_patch(
        source,
        &source_id,
        lifecycle_generation,
        expected_revision,
        requested_paths,
        &token,
        admission,
        profile_guard,
    );
    match result {
        Ok(patch) => {
            if !publication_is_current(admission, &source_id, lifecycle_generation, Some(&token)) {
                return;
            }
            let Some(snapshot) = source.snapshot.as_mut() else {
                emit_metadata_failure(
                    reservation,
                    admission,
                    source_id,
                    lifecycle_generation,
                    expected_revision,
                    request_id,
                    CatalogError::SnapshotUnavailable,
                );
                return;
            };
            for path in &patch.requested_paths {
                snapshot.wav_metadata.remove(path);
            }
            for path in &patch.manifest_paths {
                snapshot.wav_metadata.remove(path);
            }
            snapshot.wav_metadata.extend(patch.metadata.clone());
            let _ = publish_if_current(
                admission,
                reservation,
                &source_id,
                lifecycle_generation,
                Some(&token),
                CatalogEvent::WavMetadataReady {
                    source_id: source_id.clone(),
                    lifecycle_generation,
                    root_identity: patch.root_identity,
                    revision: patch.revision,
                    request_id,
                    requested_paths: patch.requested_paths,
                    metadata: patch.metadata,
                },
            );
        }
        Err(error) => {
            emit_metadata_failure(
                reservation,
                admission,
                source_id,
                lifecycle_generation,
                expected_revision,
                request_id,
                error,
            );
        }
    }
}

fn read_snapshot(
    source_id: SourceId,
    expected_revision: Option<u64>,
    profile_guard: &WritableProfileGuard,
    reservation: &mut EventReservation<'_>,
    admission: &Arc<Mutex<AdmissionState>>,
    sources: &mut HashMap<SourceId, RegisteredSource>,
) {
    let Some(source) = sources.get_mut(&source_id) else {
        emit_failure(
            reservation,
            admission,
            Some(source_id.clone()),
            None,
            CatalogOperation::ReadSnapshot,
            CatalogError::NotRegistered(source_id),
        );
        return;
    };
    let lifecycle_generation = source.lifecycle_generation;
    if let Err(error) = profile_guard.validate_current() {
        emit_failure(
            reservation,
            admission,
            Some(source_id),
            Some(source.lifecycle_generation),
            CatalogOperation::ReadSnapshot,
            CatalogError::ProfileUnavailable(error.to_string()),
        );
        return;
    }
    if let Err(error) = source
        .database
        .validate_profile_guard(profile_guard)
        .map_err(|error| CatalogError::ProfileUnavailable(error.to_string()))
    {
        emit_failure(
            reservation,
            admission,
            Some(source_id),
            Some(source.lifecycle_generation),
            CatalogOperation::ReadSnapshot,
            error,
        );
        return;
    }
    let identity = match source_root_identity(&source.root) {
        Ok(identity) => identity,
        Err(error) => {
            emit_failure(
                reservation,
                admission,
                Some(source_id),
                Some(source.lifecycle_generation),
                CatalogOperation::ReadSnapshot,
                error,
            );
            return;
        }
    };
    let result = read_snapshot_from_database(
        &source.database,
        profile_guard,
        &source_id,
        source.lifecycle_generation,
        &identity,
        || !publication_is_current(admission, &source_id, lifecycle_generation, None),
    );
    match result {
        Ok(mut snapshot) => {
            if let Some(expected) = expected_revision
                && expected != snapshot.committed_revision
            {
                emit_failure(
                    reservation,
                    admission,
                    Some(source_id),
                    Some(source.lifecycle_generation),
                    CatalogOperation::ReadSnapshot,
                    CatalogError::StaleRevision {
                        expected,
                        actual: snapshot.committed_revision,
                    },
                );
                return;
            }
            if let Err(error) = attach_wav_metadata(
                &mut snapshot,
                &source.root,
                &source.database,
                profile_guard,
                &source_id,
                &identity,
                || !publication_is_current(admission, &source_id, lifecycle_generation, None),
            ) {
                emit_failure(
                    reservation,
                    admission,
                    Some(source_id),
                    Some(source.lifecycle_generation),
                    CatalogOperation::ReadSnapshot,
                    error,
                );
                return;
            }
            source.known_root_identity = Some(snapshot.root_identity.clone());
            source.snapshot = Some(snapshot.clone());
            let _ = publish_if_current(
                admission,
                reservation,
                &source_id,
                lifecycle_generation,
                None,
                CatalogEvent::SnapshotReady { snapshot },
            );
        }
        Err(error) => emit_failure(
            reservation,
            admission,
            Some(source_id),
            Some(source.lifecycle_generation),
            CatalogOperation::ReadSnapshot,
            error,
        ),
    }
}

fn remove_source(
    source_id: SourceId,
    lifecycle_generation: u64,
    reservation: &mut EventReservation<'_>,
    admission: &Arc<Mutex<AdmissionState>>,
    sources: &mut HashMap<SourceId, RegisteredSource>,
) {
    let Some(source) = sources.get(&source_id) else {
        let _ =
            publish_removal_if_current(admission, reservation, &source_id, lifecycle_generation);
        return;
    };
    if let Err(error) = ensure_generation(
        &source_id,
        source.lifecycle_generation,
        lifecycle_generation,
    ) {
        emit_failure(
            reservation,
            admission,
            Some(source_id),
            Some(lifecycle_generation),
            CatalogOperation::Remove,
            error,
        );
        return;
    }
    sources.remove(&source_id);
    let _ = publish_removal_if_current(admission, reservation, &source_id, lifecycle_generation);
}

struct CatalogScanWriter<'a> {
    profile_guard: &'a WritableProfileGuard,
    database: &'a SourceDatabase,
    cancel: Option<&'a AtomicBool>,
}

impl ScanWriter for CatalogScanWriter<'_> {
    type Guard = ();

    fn lock(&self, _phase: ScanWritePhase) -> Self::Guard {}

    fn try_lock(&self, _phase: ScanWritePhase) -> Result<Self::Guard, ScanError> {
        if self
            .cancel
            .is_some_and(|cancel| cancel.load(Ordering::Acquire))
        {
            return Err(ScanError::Canceled);
        }
        self.validate_authority()
    }

    fn try_lock_cleanup(&self) -> Result<Self::Guard, ScanError> {
        self.validate_authority()
    }
}

impl CatalogScanWriter<'_> {
    fn validate_authority(&self) -> Result<(), ScanError> {
        self.profile_guard
            .validate_current()
            .map_err(|error| ScanError::Io {
                path: self.profile_guard.profile_root().to_path_buf(),
                source: io::Error::other(error.to_string()),
            })?;
        self.database
            .validate_profile_guard(self.profile_guard)
            .map_err(|error| ScanError::Io {
                path: self.profile_guard.profile_root().to_path_buf(),
                source: io::Error::other(error.to_string()),
            })?;
        Ok(())
    }
}

fn maintain_catalog_database(
    database: &SourceDatabase,
    profile_guard: &WritableProfileGuard,
    cancel: Option<&AtomicBool>,
    should_cancel: impl Fn() -> bool,
) -> Result<(), CatalogError> {
    let writer = CatalogScanWriter {
        profile_guard,
        database,
        cancel,
    };

    loop {
        if should_cancel() {
            return Err(CatalogError::Canceled);
        }
        let generations = {
            let _guard = writer
                .try_lock_cleanup()
                .map_err(catalog_maintenance_error)?;
            database
                .staged_source_directory_truth_generations(SOURCE_DIRECTORY_TRUTH_BATCH_LIMIT)
                .map_err(catalog_error_from_db)?
        };
        if generations.is_empty() {
            break;
        }
        for generation in generations {
            loop {
                if should_cancel() {
                    return Err(CatalogError::Canceled);
                }
                let cleanup = {
                    let _guard = writer
                        .try_lock_cleanup()
                        .map_err(catalog_maintenance_error)?;
                    database
                        .discard_source_directory_truth_generation_batch(
                            generation,
                            SOURCE_DIRECTORY_TRUTH_BATCH_LIMIT,
                        )
                        .map_err(catalog_error_from_db)?
                };
                if !cleanup.more_work {
                    break;
                }
            }
        }
    }

    loop {
        if should_cancel() {
            return Err(CatalogError::Canceled);
        }
        let cleanup = {
            let _guard = writer
                .try_lock_cleanup()
                .map_err(catalog_maintenance_error)?;
            database
                .cleanup_source_directory_truth(SOURCE_DIRECTORY_TRUTH_CLEANUP_LIMIT)
                .map_err(catalog_error_from_db)?
        };
        if !cleanup.more_work {
            return Ok(());
        }
    }
}

fn catalog_maintenance_error(error: ScanError) -> CatalogError {
    match error {
        ScanError::Canceled => CatalogError::Canceled,
        ScanError::Io { source, .. } => CatalogError::ProfileUnavailable(source.to_string()),
        error => CatalogError::Database(error.to_string()),
    }
}

fn audit_source(
    source: &mut RegisteredSource,
    profile_guard: &WritableProfileGuard,
    source_id: &SourceId,
    token: &Arc<AtomicBool>,
) -> Result<CatalogSnapshot, CatalogError> {
    profile_guard
        .validate_current()
        .map_err(|error| CatalogError::ProfileUnavailable(error.to_string()))?;
    source
        .database
        .validate_profile_guard(profile_guard)
        .map_err(|error| CatalogError::ProfileUnavailable(error.to_string()))?;
    let before_identity = source_root_identity(&source.root)?;
    if let Some(known) = &source.known_root_identity
        && known != &before_identity
    {
        return Err(CatalogError::StaleRootGeneration {
            source_id: source_id.clone(),
            detail: String::from("root identity differs from the retained catalog snapshot"),
        });
    }
    let writer = CatalogScanWriter {
        profile_guard,
        database: &source.database,
        cancel: Some(token),
    };
    let audit = audit_manifest_for_catalog_with_budget_and_progress_and_writer(
        &source.database,
        Some(token),
        now_epoch_seconds(),
        &mut |_, _| {},
        &writer,
        None,
    )
    .map_err(|error| scan_error(source_id, error))?;
    if token.load(Ordering::Acquire) {
        return Err(CatalogError::Canceled);
    }
    profile_guard
        .validate_current()
        .map_err(|error| CatalogError::ProfileUnavailable(error.to_string()))?;
    source
        .database
        .validate_profile_guard(profile_guard)
        .map_err(|error| CatalogError::ProfileUnavailable(error.to_string()))?;
    let after_identity = source_root_identity(&source.root)?;
    let audit_identity = audit.audit_commit.committed_root_identity().clone();
    let audit_revision = audit.audit_commit.committed_source_revision();
    if audit_identity != before_identity || audit_identity != after_identity {
        return Err(CatalogError::StaleRootGeneration {
            source_id: source_id.clone(),
            detail: String::from("root identity changed during authoritative audit"),
        });
    }
    source
        .database
        .validate_profile_guard(profile_guard)
        .map_err(|error| CatalogError::ProfileUnavailable(error.to_string()))?;
    let mut snapshot = read_snapshot_from_database(
        &source.database,
        profile_guard,
        source_id,
        source.lifecycle_generation,
        &audit_identity,
        || token.load(Ordering::Acquire),
    )?;
    if snapshot.committed_revision != audit_revision {
        return Err(CatalogError::Audit(format!(
            "audit committed revision {audit_revision}, snapshot read revision {}",
            snapshot.committed_revision
        )));
    }
    attach_wav_metadata(
        &mut snapshot,
        &source.root,
        &source.database,
        profile_guard,
        source_id,
        &audit_identity,
        || token.load(Ordering::Acquire),
    )?;
    Ok(snapshot)
}

fn attach_wav_metadata(
    snapshot: &mut CatalogSnapshot,
    root: &Path,
    database: &SourceDatabase,
    profile_guard: &WritableProfileGuard,
    source_id: &SourceId,
    expected_root_identity: &RootIdentity,
    mut should_cancel: impl FnMut() -> bool,
) -> Result<(), CatalogError> {
    let metadata = read_wav_display_metadata_for_manifest(
        root,
        expected_root_identity,
        &snapshot.manifest,
        &mut should_cancel,
    )
    .map_err(|error| scan_wav_metadata_error(source_id, error))?;
    if should_cancel() {
        return Err(CatalogError::Canceled);
    }
    database
        .validate_profile_guard(profile_guard)
        .map_err(|error| CatalogError::ProfileUnavailable(error.to_string()))?;
    if should_cancel() {
        return Err(CatalogError::Canceled);
    }
    let current_revision = database.get_revision().map_err(catalog_error_from_db)?;
    if current_revision != snapshot.committed_revision {
        return Err(CatalogError::StaleRevision {
            expected: snapshot.committed_revision,
            actual: current_revision,
        });
    }
    snapshot.wav_metadata = metadata
        .into_iter()
        .map(|(path, metadata)| {
            (
                path,
                CatalogWavMetadata {
                    frames: metadata.frames,
                    sample_rate: metadata.sample_rate,
                },
            )
        })
        .collect();
    Ok(())
}

fn read_snapshot_from_database(
    database: &SourceDatabase,
    profile_guard: &WritableProfileGuard,
    source_id: &SourceId,
    lifecycle_generation: u64,
    root_identity: &RootIdentity,
    mut should_cancel: impl FnMut() -> bool,
) -> Result<CatalogSnapshot, CatalogError> {
    database
        .validate_profile_guard(profile_guard)
        .map_err(|error| CatalogError::ProfileUnavailable(error.to_string()))?;
    let Some(audit_metadata) = database
        .last_authoritative_audit_metadata()
        .map_err(catalog_error_from_db)?
    else {
        return Err(CatalogError::SnapshotUnavailable);
    };
    if &audit_metadata.root_identity != root_identity {
        return Err(CatalogError::StaleRootGeneration {
            source_id: source_id.clone(),
            detail: String::from("persisted catalog root identity does not match source root"),
        });
    }
    let (revision, mut manifest) = database
        .manifest_snapshot_with_revision()
        .map_err(catalog_error_from_db)?;
    if audit_metadata.source_revision != revision {
        return Err(CatalogError::DirectoryTruthUnavailable(format!(
            "audit metadata revision {} does not match manifest revision {revision}",
            audit_metadata.source_revision
        )));
    }
    // Keep the retained projection ordered by the native path comparator so bounded exact-path
    // patches can validate their subset with binary search. The database query is ordered by its
    // UTF-8 text key, whose punctuation order differs from component-aware Path ordering for
    // cases such as `a.wav` and `a/file.wav`.
    manifest.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    if should_cancel() {
        return Err(CatalogError::Canceled);
    }
    let first_page = database
        .source_directory_truth_page(None, DIRECTORY_PAGE_LIMIT)
        .map_err(catalog_error_from_db)?;
    let (directory_generation, mut directories, mut cursor) = match first_page.state {
        SourceDirectoryTruthState::Active {
            generation,
            published_source_revision,
        } if published_source_revision == revision => {
            (generation, first_page.entries, first_page.next_cursor)
        }
        state => {
            return Err(CatalogError::DirectoryTruthUnavailable(format!(
                "state {state:?} does not publish manifest revision {revision}"
            )));
        }
    };
    while let Some(next) = cursor.take() {
        if should_cancel() {
            return Err(CatalogError::Canceled);
        }
        let page: SourceDirectoryTruthPage = database
            .source_directory_truth_page(Some(&next), DIRECTORY_PAGE_LIMIT)
            .map_err(catalog_error_from_db)?;
        match page.state {
            SourceDirectoryTruthState::Active {
                generation,
                published_source_revision,
            } if generation == directory_generation && published_source_revision == revision => {}
            state => {
                return Err(CatalogError::DirectoryTruthUnavailable(format!(
                    "page state {state:?} does not match revision {revision}"
                )));
            }
        }
        directories.extend(page.entries);
        cursor = page.next_cursor;
    }
    let final_revision = database.get_revision().map_err(catalog_error_from_db)?;
    if final_revision != revision {
        return Err(CatalogError::StaleRevision {
            expected: revision,
            actual: final_revision,
        });
    }
    database
        .validate_profile_guard(profile_guard)
        .map_err(|error| CatalogError::ProfileUnavailable(error.to_string()))?;
    let curation_metadata = curation::load_snapshot_curation_metadata(
        database,
        revision,
        &manifest,
        &mut should_cancel,
    )?;
    Ok(CatalogSnapshot {
        source_id: source_id.clone(),
        root_identity: root_identity.clone(),
        lifecycle_generation,
        committed_revision: revision,
        directory_generation,
        manifest,
        directories,
        wav_metadata: BTreeMap::new(),
        curation_metadata,
    })
}

fn validate_canonical_root(root: &Path) -> Result<(), CatalogError> {
    if !root.is_absolute()
        || root
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(CatalogError::InvalidRoot(root.to_path_buf()));
    }
    Ok(())
}

fn source_root_identity(root: &Path) -> Result<RootIdentity, CatalogError> {
    let metadata = fs::metadata(root).map_err(|error| CatalogError::StaleRootGeneration {
        source_id: SourceId::from_string("unknown"),
        detail: format!("could not inspect source root {}: {error}", root.display()),
    })?;
    if !metadata.is_dir() {
        return Err(CatalogError::InvalidRoot(root.to_path_buf()));
    }
    let identity = stable_filesystem_identity(root, &metadata).ok_or_else(|| {
        CatalogError::StaleRootGeneration {
            source_id: SourceId::from_string("unknown"),
            detail: format!("stable identity unavailable for {}", root.display()),
        }
    })?;
    Ok(RootIdentity::from_bytes(identity.into_bytes()))
}

fn catalog_profile_child(source_id: &SourceId) -> PathBuf {
    let digest = blake3::hash(source_id.as_str().as_bytes()).to_hex();
    PathBuf::from(format!("{CATALOG_CHILD_PREFIX}{digest}"))
}

fn ensure_generation(
    source_id: &SourceId,
    expected: u64,
    received: u64,
) -> Result<(), CatalogError> {
    if expected == received {
        Ok(())
    } else {
        Err(CatalogError::StaleLifecycleGeneration {
            source_id: source_id.clone(),
            expected,
            received,
        })
    }
}

fn emit_failure(
    reservation: &mut EventReservation<'_>,
    admission: &Arc<Mutex<AdmissionState>>,
    source_id: Option<SourceId>,
    lifecycle_generation: Option<u64>,
    operation: CatalogOperation,
    error: CatalogError,
) {
    if let (Some(source_id), Some(lifecycle_generation)) =
        (source_id.as_ref(), lifecycle_generation)
    {
        let _ = publish_if_current(
            admission,
            reservation,
            source_id,
            lifecycle_generation,
            None,
            CatalogEvent::Failed {
                source_id: Some(source_id.clone()),
                lifecycle_generation: Some(lifecycle_generation),
                operation,
                error,
            },
        );
    } else {
        reservation.publish(CatalogEvent::Failed {
            source_id,
            lifecycle_generation,
            operation,
            error,
        });
    }
}

fn emit_metadata_failure(
    reservation: &mut EventReservation<'_>,
    admission: &Arc<Mutex<AdmissionState>>,
    source_id: SourceId,
    lifecycle_generation: u64,
    expected_revision: u64,
    request_id: u64,
    error: CatalogError,
) {
    let _ = publish_if_current(
        admission,
        reservation,
        &source_id,
        lifecycle_generation,
        None,
        CatalogEvent::WavMetadataFailed {
            source_id: source_id.clone(),
            lifecycle_generation,
            expected_revision,
            request_id,
            error,
        },
    );
}

fn emit_tag_page_failure(
    reservation: &mut EventReservation<'_>,
    admission: &Arc<Mutex<AdmissionState>>,
    source_id: SourceId,
    lifecycle_generation: u64,
    expected_revision: u64,
    request_id: u64,
    token: Option<&Arc<AtomicBool>>,
    error: CatalogError,
) {
    let observed_revision = match &error {
        CatalogError::StaleRevision { actual, .. } => Some(*actual),
        _ => None,
    };
    let _ = publish_if_current(
        admission,
        reservation,
        &source_id,
        lifecycle_generation,
        token,
        CatalogEvent::TagPageFailed {
            source_id: source_id.clone(),
            lifecycle_generation,
            expected_revision,
            observed_revision,
            request_id,
            error,
        },
    );
}

fn catalog_error_from_db(error: SourceDbError) -> CatalogError {
    CatalogError::Database(error.to_string())
}

fn scan_error(
    source_id: &SourceId,
    error: wavecrate_scan::sample_sources::scanner::ScanError,
) -> CatalogError {
    match error {
        wavecrate_scan::sample_sources::scanner::ScanError::Canceled => CatalogError::Canceled,
        error => CatalogError::Audit(format!("{source_id}: {error}")),
    }
}

fn scan_wav_metadata_error(
    source_id: &SourceId,
    error: wavecrate_scan::sample_sources::scanner::ScanError,
) -> CatalogError {
    match error {
        wavecrate_scan::sample_sources::scanner::ScanError::Canceled => CatalogError::Canceled,
        wavecrate_scan::sample_sources::scanner::ScanError::StaleRootGeneration { root } => {
            CatalogError::StaleRootGeneration {
                source_id: source_id.clone(),
                detail: format!(
                    "source root changed while reading WAV metadata: {}",
                    root.display()
                ),
            }
        }
        error => CatalogError::Audit(format!("{source_id}: {error}")),
    }
}

fn now_epoch_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{MutexGuard, OnceLock};
    use std::thread;
    use tempfile::TempDir;
    use wavecrate_library::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard};

    static TEST_LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();

    struct Fixture {
        _serial: MutexGuard<'static, ()>,
        _base: ConfigBaseGuard,
        _config_root: TempDir,
        _profile: PersistenceProfileGuard,
        guard: WritableProfileGuard,
        root: TempDir,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let serial = TEST_LOCK
                .get_or_init(|| std::sync::Mutex::new(()))
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = tempfile::tempdir().expect("source root");
            let config_root = tempfile::tempdir().expect("config base");
            let base = ConfigBaseGuard::set(
                fs::canonicalize(config_root.path()).expect("canonical config base"),
            );
            let profile = PersistenceProfileGuard::named(name);
            let guard = WritableProfileGuard::acquire_current().expect("profile guard");
            guard.validate_current().expect("profile validation");
            guard.try_clone().expect("profile clone");
            Self {
                _serial: serial,
                _base: base,
                _config_root: config_root,
                _profile: profile,
                guard,
                root,
            }
        }
    }

    fn receive_until(
        service: &CatalogService,
        predicate: impl Fn(&CatalogEvent) -> bool,
    ) -> CatalogEvent {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let event = service
                .recv_event_timeout(Duration::from_millis(50))
                .expect("catalog event");
            if predicate(&event) {
                return event;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "unexpected catalog event: {event:?}"
            );
        }
    }

    fn wait_for_tag_page_input_release(service: &CatalogService) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if admission_state(&service.admission, |state| state.tag_page_input_bytes) == 0 {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "tag-page input reservation was not released"
            );
            thread::yield_now();
        }
    }

    fn register_and_audit(
        service: &CatalogService,
        root: &Path,
        source_id: &SourceId,
    ) -> CatalogSnapshot {
        service
            .submit(CatalogCommand::Register {
                source_id: source_id.clone(),
                canonical_root: root.to_path_buf(),
                lifecycle_generation: 1,
            })
            .expect("register");
        let _ = receive_until(service, |event| {
            matches!(event, CatalogEvent::Registered { .. })
        });
        service
            .submit(CatalogCommand::RequestAudit {
                source_id: source_id.clone(),
                lifecycle_generation: 1,
            })
            .expect("audit");
        match receive_until(service, |event| {
            matches!(event, CatalogEvent::AuditCommitted { .. })
        }) {
            CatalogEvent::AuditCommitted { snapshot, .. } => snapshot,
            _ => unreachable!(),
        }
    }

    fn write_test_wav(path: &Path, sample_rate: u32, frames: u32) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(path, spec).expect("create test WAV");
        for sample in 0..frames {
            writer
                .write_sample(sample as i16)
                .expect("write test WAV sample");
        }
        writer.finalize().expect("finalize test WAV");
    }

    fn apply_curation_updates(
        service: &CatalogService,
        source_id: &SourceId,
        snapshot: &CatalogSnapshot,
        request_id: u64,
        updates: Vec<SourceCurationUpdate>,
    ) -> SourceCurationCommitReceipt {
        service
            .submit(CatalogCommand::ApplyCuration {
                source_id: source_id.clone(),
                lifecycle_generation: snapshot.lifecycle_generation,
                expected_revision: snapshot.committed_revision,
                root_identity: snapshot.root_identity.clone(),
                request_id,
                updates,
            })
            .expect("curation admission");
        match receive_until(service, |event| {
            matches!(
                event,
                CatalogEvent::CurationCommitted {
                    request_id: received,
                    ..
                } if *received == request_id
            )
        }) {
            CatalogEvent::CurationCommitted { receipt, .. } => receipt,
            _ => unreachable!(),
        }
    }

    #[test]
    fn bounded_tag_pages_publish_selected_and_literal_vocabulary_results() {
        let fixture = Fixture::new("catalog-tag-pages");
        write_test_wav(&fixture.root.path().join("one.wav"), 16_000, 4);
        write_test_wav(&fixture.root.path().join("two.wav"), 16_000, 4);
        let source_id = SourceId::from_string("source-tag-pages");
        let service = CatalogService::start(&fixture.guard).expect("catalog service");
        let snapshot = register_and_audit(&service, fixture.root.path(), &source_id);
        let one =
            RootRelativePath::try_from_path(PathBuf::from("one.wav")).expect("relative one path");
        let two =
            RootRelativePath::try_from_path(PathBuf::from("two.wav")).expect("relative two path");
        let receipt = apply_curation_updates(
            &service,
            &source_id,
            &snapshot,
            700,
            vec![
                SourceCurationUpdate::AddTag {
                    path: one.clone(),
                    label: String::from("shared"),
                },
                SourceCurationUpdate::AddTag {
                    path: two.clone(),
                    label: String::from("shared"),
                },
                SourceCurationUpdate::AddTag {
                    path: one.clone(),
                    label: String::from("only-one"),
                },
                SourceCurationUpdate::AddTag {
                    path: one,
                    label: String::from("foo%literal"),
                },
                SourceCurationUpdate::AddTag {
                    path: two,
                    label: String::from("foo_other"),
                },
            ],
        );
        let revision = receipt.source_revision();

        service
            .submit(CatalogCommand::RequestTagPage {
                source_id: source_id.clone(),
                lifecycle_generation: 1,
                expected_revision: revision,
                request_id: 701,
                query: CatalogTagPageQuery::SelectedPaths {
                    relative_paths: vec![PathBuf::from("one.wav"), PathBuf::from("two.wav")],
                },
                after_cursor: None,
                page_limit: 2,
            })
            .expect("selected tag page request");
        let first = match receive_until(&service, |event| {
            matches!(
                event,
                CatalogEvent::TagPageReady {
                    request_id: 701,
                    ..
                }
            )
        }) {
            CatalogEvent::TagPageReady {
                source_id: event_source,
                lifecycle_generation,
                root_identity,
                revision: event_revision,
                request_id: 701,
                page,
            } => {
                assert_eq!(event_source, source_id);
                assert_eq!(lifecycle_generation, 1);
                assert_eq!(root_identity, snapshot.root_identity);
                assert_eq!(event_revision, revision);
                assert_eq!(page.revision, revision);
                assert_eq!(page.entries.len(), 2);
                assert!(
                    page.entries
                        .iter()
                        .all(|entry| entry.assignment_count.is_some())
                );
                page
            }
            _ => unreachable!(),
        };
        wait_for_tag_page_input_release(&service);
        let first_ids = first
            .entries
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>();
        let first_cursor = first.next_cursor.expect("selected continuation");
        service
            .submit(CatalogCommand::RequestTagPage {
                source_id: source_id.clone(),
                lifecycle_generation: 1,
                expected_revision: revision,
                request_id: 702,
                query: CatalogTagPageQuery::SelectedPaths {
                    relative_paths: vec![PathBuf::from("two.wav"), PathBuf::from("one.wav")],
                },
                after_cursor: Some(first_cursor),
                page_limit: 2,
            })
            .expect("selected continuation request");
        let second = match receive_until(&service, |event| {
            matches!(
                event,
                CatalogEvent::TagPageReady {
                    request_id: 702,
                    ..
                }
            )
        }) {
            CatalogEvent::TagPageReady { page, .. } => page,
            _ => unreachable!(),
        };
        assert_eq!(second.revision, revision);
        assert!(second.next_cursor.is_none());
        assert_eq!(second.entries.len(), 2);
        assert!(
            second
                .entries
                .iter()
                .all(|entry| !first_ids.contains(&entry.id))
        );

        service
            .submit(CatalogCommand::RequestTagPage {
                source_id: source_id.clone(),
                lifecycle_generation: 1,
                expected_revision: revision,
                request_id: 703,
                query: CatalogTagPageQuery::VocabularyPrefix {
                    prefix: String::from("foo%"),
                },
                after_cursor: None,
                page_limit: SOURCE_TAG_PAGE_MAX_PAGE_SIZE,
            })
            .expect("literal vocabulary page request");
        let vocabulary = match receive_until(&service, |event| {
            matches!(
                event,
                CatalogEvent::TagPageReady {
                    request_id: 703,
                    ..
                }
            )
        }) {
            CatalogEvent::TagPageReady { page, .. } => page,
            _ => unreachable!(),
        };
        assert_eq!(vocabulary.revision, revision);
        assert_eq!(vocabulary.entries.len(), 1);
        assert_eq!(vocabulary.entries[0].normalized_text, "foo%literal");
        assert_eq!(vocabulary.entries[0].assignment_count, None);
        service.shutdown();
    }

    #[test]
    fn tag_page_stale_revision_reports_observed_revision() {
        let fixture = Fixture::new("catalog-tag-page-stale");
        write_test_wav(&fixture.root.path().join("one.wav"), 16_000, 4);
        let source_id = SourceId::from_string("source-tag-page-stale");
        let service = CatalogService::start(&fixture.guard).expect("catalog service");
        let snapshot = register_and_audit(&service, fixture.root.path(), &source_id);
        let receipt = apply_curation_updates(
            &service,
            &source_id,
            &snapshot,
            710,
            vec![SourceCurationUpdate::AddTag {
                path: RootRelativePath::try_from_path(PathBuf::from("one.wav"))
                    .expect("relative path"),
                label: String::from("stale-check"),
            }],
        );
        let current_revision = receipt.source_revision();
        service
            .submit(CatalogCommand::RequestTagPage {
                source_id: source_id.clone(),
                lifecycle_generation: 1,
                expected_revision: snapshot.committed_revision,
                request_id: 711,
                query: CatalogTagPageQuery::VocabularyPrefix {
                    prefix: String::new(),
                },
                after_cursor: None,
                page_limit: 1,
            })
            .expect("stale tag page request");
        let event = receive_until(&service, |event| {
            matches!(
                event,
                CatalogEvent::TagPageFailed {
                    request_id: 711,
                    ..
                }
            )
        });
        assert!(matches!(
            event,
            CatalogEvent::TagPageFailed {
                source_id: event_source,
                lifecycle_generation: 1,
                expected_revision,
                observed_revision: Some(observed),
                request_id: 711,
                error: CatalogError::StaleRevision { expected, actual },
            } if event_source == source_id
                && expected_revision == snapshot.committed_revision
                && expected == snapshot.committed_revision
                && observed == current_revision
                && actual == current_revision
        ));
        service.shutdown();
    }

    #[test]
    fn tag_page_admission_rejects_unbounded_or_unsafe_input_but_allows_empty_selection() {
        let fixture = Fixture::new("catalog-tag-page-admission");
        write_test_wav(&fixture.root.path().join("one.wav"), 16_000, 4);
        let source_id = SourceId::from_string("source-tag-page-admission");
        let service = CatalogService::start(&fixture.guard).expect("catalog service");
        let snapshot = register_and_audit(&service, fixture.root.path(), &source_id);
        let request = |query, page_limit| CatalogCommand::RequestTagPage {
            source_id: source_id.clone(),
            lifecycle_generation: 1,
            expected_revision: snapshot.committed_revision,
            request_id: 720,
            query,
            after_cursor: None,
            page_limit,
        };

        let too_many = (0..=SOURCE_TAG_PAGE_MAX_PATHS)
            .map(|index| PathBuf::from(format!("{index}.wav")))
            .collect();
        assert_eq!(
            service.submit(request(
                CatalogTagPageQuery::SelectedPaths {
                    relative_paths: too_many,
                },
                1,
            )),
            Err(CatalogSubmitError::InvalidTagPageRequest(
                CatalogTagPageRequestError::TooManyPaths {
                    max: SOURCE_TAG_PAGE_MAX_PATHS,
                    actual: SOURCE_TAG_PAGE_MAX_PATHS + 1,
                }
            ))
        );
        let too_many_bytes = vec![PathBuf::from(
            "x".repeat(SOURCE_TAG_PAGE_MAX_PATH_BYTES + 1),
        )];
        assert!(matches!(
            service.submit(request(
                CatalogTagPageQuery::SelectedPaths {
                    relative_paths: too_many_bytes,
                },
                1,
            )),
            Err(CatalogSubmitError::InvalidTagPageRequest(
                CatalogTagPageRequestError::PathBytesTooLarge { .. }
            ))
        ));
        assert!(matches!(
            service.submit(request(
                CatalogTagPageQuery::SelectedPaths {
                    relative_paths: vec![PathBuf::from("../escape.wav")],
                },
                1,
            )),
            Err(CatalogSubmitError::InvalidTagPageRequest(
                CatalogTagPageRequestError::InvalidPath {
                    reason: wavecrate_library::sample_sources::reconciliation::RootRelativePathError::ParentTraversal,
                    ..
                }
            ))
        ));
        assert!(matches!(
            service.submit(request(
                CatalogTagPageQuery::VocabularyPrefix {
                    prefix: "p".repeat(SOURCE_TAG_PAGE_MAX_PREFIX_BYTES + 1),
                },
                1,
            )),
            Err(CatalogSubmitError::InvalidTagPageRequest(
                CatalogTagPageRequestError::PrefixTooLong { .. }
            ))
        ));
        assert_eq!(
            service.submit(request(
                CatalogTagPageQuery::VocabularyPrefix {
                    prefix: String::from("tag\0prefix"),
                },
                1,
            )),
            Err(CatalogSubmitError::InvalidTagPageRequest(
                CatalogTagPageRequestError::PrefixContainsNul
            ))
        );
        assert!(matches!(
            service.submit(request(
                CatalogTagPageQuery::VocabularyPrefix {
                    prefix: String::new(),
                },
                0,
            )),
            Err(CatalogSubmitError::InvalidTagPageRequest(
                CatalogTagPageRequestError::InvalidPageLimit { actual: 0, .. }
            ))
        ));
        assert!(matches!(
            service.submit(request(
                CatalogTagPageQuery::VocabularyPrefix {
                    prefix: String::new(),
                },
                SOURCE_TAG_PAGE_MAX_PAGE_SIZE + 1,
            )),
            Err(CatalogSubmitError::InvalidTagPageRequest(
                CatalogTagPageRequestError::InvalidPageLimit { .. }
            ))
        ));

        service
            .submit(request(
                CatalogTagPageQuery::SelectedPaths {
                    relative_paths: Vec::new(),
                },
                1,
            ))
            .expect("empty selected query admission");
        let event = receive_until(&service, |event| {
            matches!(
                event,
                CatalogEvent::TagPageReady {
                    request_id: 720,
                    ..
                }
            )
        });
        assert!(matches!(
            event,
            CatalogEvent::TagPageReady {
                source_id: event_source,
                page,
                ..
            } if event_source == source_id && page.entries.is_empty()
        ));
        service.shutdown();
    }

    #[test]
    fn tag_page_input_admission_accounts_path_container_overhead_and_releases_after_failure() {
        let fixture = Fixture::new("catalog-tag-page-input-budget");
        write_test_wav(&fixture.root.path().join("one.wav"), 16_000, 4);
        let source_id = SourceId::from_string("source-tag-page-input-budget");
        let (service, test_gate) =
            CatalogService::start_with_test_gate(&fixture.guard).expect("catalog service");
        let snapshot = register_and_audit(&service, fixture.root.path(), &source_id);
        let pause = test_gate.pause_next();
        let giant_path = PathBuf::from("x".repeat(SOURCE_TAG_PAGE_MAX_PATH_BYTES));
        let request = |request_id| CatalogCommand::RequestTagPage {
            source_id: source_id.clone(),
            lifecycle_generation: 1,
            expected_revision: snapshot.committed_revision,
            request_id,
            query: CatalogTagPageQuery::SelectedPaths {
                relative_paths: vec![giant_path.clone()],
            },
            after_cursor: None,
            page_limit: 1,
        };
        service
            .submit(request(750))
            .expect("first input reservation");
        let expected_bytes = selected_paths_input_bytes(std::slice::from_ref(&giant_path));
        assert_eq!(
            admission_state(&service.admission, |state| state.tag_page_input_bytes),
            expected_bytes
        );
        assert!(expected_bytes > SOURCE_TAG_PAGE_MAX_PATH_BYTES);
        assert_eq!(service.submit(request(751)), Err(CatalogSubmitError::Full));
        assert_eq!(
            admission_state(&service.admission, |state| state.tag_page_input_bytes),
            expected_bytes
        );
        pause.release();

        let event = service
            .recv_event_timeout(Duration::from_secs(5))
            .expect("tag page failure");
        assert!(matches!(
            event,
            CatalogEvent::TagPageFailed {
                request_id: 750,
                error: CatalogError::TagPagePathMissing { .. },
                ..
            }
        ));
        wait_for_tag_page_input_release(&service);
        service.shutdown();
    }

    #[test]
    fn obsolete_tag_page_generation_is_discarded_and_event_credit_is_released() {
        let fixture = Fixture::new("catalog-tag-page-generation");
        write_test_wav(&fixture.root.path().join("one.wav"), 16_000, 4);
        let source_id = SourceId::from_string("source-tag-page-generation");
        let (service, test_gate) =
            CatalogService::start_with_test_gate(&fixture.guard).expect("catalog service");
        let snapshot = register_and_audit(&service, fixture.root.path(), &source_id);
        let pause = test_gate.pause_next();
        service
            .submit(CatalogCommand::RequestTagPage {
                source_id: source_id.clone(),
                lifecycle_generation: 1,
                expected_revision: snapshot.committed_revision,
                request_id: 730,
                query: CatalogTagPageQuery::SelectedPaths {
                    relative_paths: vec![PathBuf::from("one.wav")],
                },
                after_cursor: None,
                page_limit: 1,
            })
            .expect("tag page admission");
        assert!(admission_state(&service.admission, |state| {
            state.tag_page_input_bytes > 0
        }));
        service
            .submit(CatalogCommand::Register {
                source_id: source_id.clone(),
                canonical_root: fixture.root.path().to_path_buf(),
                lifecycle_generation: 2,
            })
            .expect("replacement registration admission");
        pause.release();

        assert!(matches!(
            receive_until(&service, |event| {
                matches!(
                    event,
                    CatalogEvent::Registered {
                        source_id: event_source,
                        lifecycle_generation: 2,
                    } if event_source == &source_id
                )
            }),
            CatalogEvent::Registered {
                lifecycle_generation: 2,
                ..
            }
        ));
        let _ = receive_until(&service, |event| {
            matches!(event, CatalogEvent::SnapshotReady { .. })
        });
        wait_for_tag_page_input_release(&service);
        service
            .submit(CatalogCommand::RequestTagPage {
                source_id: source_id.clone(),
                lifecycle_generation: 2,
                expected_revision: snapshot.committed_revision,
                request_id: 731,
                query: CatalogTagPageQuery::VocabularyPrefix {
                    prefix: String::new(),
                },
                after_cursor: None,
                page_limit: 1,
            })
            .expect("post-discard tag page admission proves credit release");
        assert!(matches!(
            receive_until(&service, |event| {
                matches!(
                    event,
                    CatalogEvent::TagPageReady {
                        request_id: 731,
                        ..
                    }
                )
            }),
            CatalogEvent::TagPageReady {
                request_id: 731,
                ..
            }
        ));
        service.shutdown();
        // The superseded request had no completion event; only the replacement request can
        // produce a page after generation 2 becomes current.
    }

    #[test]
    fn queued_tag_page_is_dropped_during_shutdown_and_worker_closes() {
        let fixture = Fixture::new("catalog-tag-page-shutdown");
        write_test_wav(&fixture.root.path().join("one.wav"), 16_000, 4);
        let source_id = SourceId::from_string("source-tag-page-shutdown");
        let (service, test_gate) =
            CatalogService::start_with_test_gate(&fixture.guard).expect("catalog service");
        let snapshot = register_and_audit(&service, fixture.root.path(), &source_id);
        let pause = test_gate.pause_next();
        service
            .submit(CatalogCommand::RequestTagPage {
                source_id,
                lifecycle_generation: 1,
                expected_revision: snapshot.committed_revision,
                request_id: 740,
                query: CatalogTagPageQuery::SelectedPaths {
                    relative_paths: vec![PathBuf::from("one.wav")],
                },
                after_cursor: None,
                page_limit: 1,
            })
            .expect("tag page admission");
        assert!(admission_state(&service.admission, |state| {
            state.tag_page_input_bytes > 0
        }));
        let service = Arc::new(service);
        let shutdown_service = Arc::clone(&service);
        let shutdown_thread = thread::spawn(move || shutdown_service.shutdown());
        while !admission_state(&service.admission, |state| state.shutdown) {
            thread::yield_now();
        }
        pause.release();
        shutdown_thread.join().expect("catalog shutdown");
        wait_for_tag_page_input_release(&service);

        let mut saw_closed = false;
        while let Ok(event) = service.try_recv_event() {
            match event {
                CatalogEvent::Closed => saw_closed = true,
                CatalogEvent::TagPageReady { .. } | CatalogEvent::TagPageFailed { .. } => {
                    panic!("shutdown dropped tag page must not publish: {event:?}")
                }
                _ => {}
            }
        }
        assert!(saw_closed);
    }

    fn curation_operation(
        source_id: &SourceId,
        lifecycle_generation: u64,
        root_identity: &RootIdentity,
        request_id: u64,
        fence: SourceCurationOperationFence,
        batches: Vec<Vec<SourceCurationUpdate>>,
    ) -> CatalogCommand {
        CatalogCommand::ApplyCurationOperation {
            source_id: source_id.clone(),
            lifecycle_generation,
            fence,
            root_identity: root_identity.clone(),
            request_id,
            batches,
        }
    }

    fn receive_curation_operation(
        service: &CatalogService,
        request_id: u64,
    ) -> SourceCurationOperationOutcome {
        match receive_until(service, |event| {
            matches!(
                event,
                CatalogEvent::CurationOperationCompleted {
                    request_id: received,
                    ..
                } if *received == request_id
            )
        }) {
            CatalogEvent::CurationOperationCompleted { outcome, .. } => outcome,
            _ => unreachable!(),
        }
    }

    fn repeated_rating_batches(
        update_count: usize,
        path: &RootRelativePath,
    ) -> Vec<Vec<SourceCurationUpdate>> {
        let updates = (0..update_count)
            .map(|_| SourceCurationUpdate::SetRating {
                path: path.clone(),
                rating: wavecrate_library::sample_sources::Rating::KEEP_1,
            })
            .collect::<Vec<_>>();
        updates
            .chunks(MAX_CURATION_UPDATES)
            .map(|batch| batch.to_vec())
            .collect()
    }

    #[test]
    fn bounded_metadata_request_publishes_exact_patch_with_terminal_paths() {
        let fixture = Fixture::new("catalog-wav-metadata-patch");
        let valid = fixture.root.path().join("visible.wav");
        let malformed = fixture.root.path().join("malformed.wav");
        let punctuation = fixture.root.path().join("a.wav");
        let nested = fixture.root.path().join("a").join("file.wav");
        fs::create_dir_all(nested.parent().expect("nested parent")).expect("create nested dir");
        write_test_wav(&valid, 16_000, 9);
        fs::write(&malformed, b"not a wave").expect("write malformed WAV");
        write_test_wav(&punctuation, 22_050, 4);
        write_test_wav(&nested, 44_100, 5);

        let source_id = SourceId::from_string("source-wav-metadata-patch");
        let service = CatalogService::start(&fixture.guard).expect("catalog service");
        let snapshot = register_and_audit(&service, fixture.root.path(), &source_id);
        let requested_paths = vec![
            PathBuf::from("visible.wav"),
            PathBuf::from("malformed.wav"),
            PathBuf::from("a.wav"),
            PathBuf::from("a/file.wav"),
            PathBuf::from("missing.wav"),
        ];
        service
            .submit(CatalogCommand::RequestWavMetadata {
                source_id: source_id.clone(),
                lifecycle_generation: 1,
                expected_revision: snapshot.committed_revision,
                request_id: 41,
                relative_paths: requested_paths.clone(),
            })
            .expect("metadata request");

        let event = receive_until(&service, |event| {
            matches!(event, CatalogEvent::WavMetadataReady { request_id: 41, .. })
        });
        assert!(matches!(
            event,
            CatalogEvent::WavMetadataReady {
                source_id: event_source,
                lifecycle_generation: 1,
                root_identity,
                revision,
                request_id: 41,
                requested_paths: event_paths,
                metadata,
            } if event_source == source_id
                && root_identity == snapshot.root_identity
                && revision == snapshot.committed_revision
                && event_paths == requested_paths
                && metadata.get(Path::new("visible.wav")) == Some(&CatalogWavMetadata {
                    frames: 9,
                    sample_rate: 16_000,
                })
                && metadata.get(Path::new("a.wav")) == Some(&CatalogWavMetadata {
                    frames: 4,
                    sample_rate: 22_050,
                })
                && metadata.get(Path::new("a/file.wav")) == Some(&CatalogWavMetadata {
                    frames: 5,
                    sample_rate: 44_100,
                })
                && !metadata.contains_key(Path::new("malformed.wav"))
                && !metadata.contains_key(Path::new("missing.wav"))
        ));
        service.shutdown();
    }

    #[test]
    fn metadata_revision_mismatch_is_a_distinct_request_failure() {
        let fixture = Fixture::new("catalog-wav-metadata-stale-revision");
        let path = fixture.root.path().join("stale.wav");
        write_test_wav(&path, 8_000, 3);

        let source_id = SourceId::from_string("source-wav-metadata-stale-revision");
        let service = CatalogService::start(&fixture.guard).expect("catalog service");
        let snapshot = register_and_audit(&service, fixture.root.path(), &source_id);
        service
            .submit(CatalogCommand::RequestWavMetadata {
                source_id: source_id.clone(),
                lifecycle_generation: 1,
                expected_revision: snapshot.committed_revision + 1,
                request_id: 42,
                relative_paths: vec![PathBuf::from("stale.wav")],
            })
            .expect("metadata request");

        let event = receive_until(&service, |event| {
            matches!(
                event,
                CatalogEvent::WavMetadataFailed { request_id: 42, .. }
            )
        });
        assert!(matches!(
            event,
            CatalogEvent::WavMetadataFailed {
                source_id: event_source,
                lifecycle_generation: 1,
                expected_revision,
                request_id: 42,
                error: CatalogError::StaleRevision { expected, actual },
            } if event_source == source_id
                && expected_revision == snapshot.committed_revision + 1
                && expected == snapshot.committed_revision + 1
                && actual == snapshot.committed_revision
        ));
        service.shutdown();
    }

    #[test]
    fn stale_metadata_lifecycle_is_fenced_without_a_completion_event() {
        let fixture = Fixture::new("catalog-wav-metadata-stale-lifecycle");
        let path = fixture.root.path().join("stale-lifecycle.wav");
        write_test_wav(&path, 8_000, 3);

        let source_id = SourceId::from_string("source-wav-metadata-stale-lifecycle");
        let service = CatalogService::start(&fixture.guard).expect("catalog service");
        let snapshot = register_and_audit(&service, fixture.root.path(), &source_id);
        service
            .submit(CatalogCommand::RequestWavMetadata {
                source_id,
                lifecycle_generation: 0,
                expected_revision: snapshot.committed_revision,
                request_id: 43,
                relative_paths: vec![PathBuf::from("stale-lifecycle.wav")],
            })
            .expect("stale metadata request admission");
        thread::sleep(Duration::from_millis(50));
        assert!(matches!(service.try_recv_event(), Err(TryRecvError::Empty)));
        service.shutdown();
    }

    #[test]
    fn metadata_request_admission_rejects_empty_invalid_and_oversized_batches() {
        let fixture = Fixture::new("catalog-wav-metadata-admission");
        let service = CatalogService::start(&fixture.guard).expect("catalog service");
        let source_id = SourceId::from_string("source-wav-metadata-admission");
        let request = |relative_paths| CatalogCommand::RequestWavMetadata {
            source_id: source_id.clone(),
            lifecycle_generation: 1,
            expected_revision: 1,
            request_id: 44,
            relative_paths,
        };

        assert_eq!(
            service.submit(request(Vec::new())),
            Err(CatalogSubmitError::InvalidWavMetadataRequest(
                CatalogWavMetadataRequestError::Empty
            ))
        );
        assert!(matches!(
            service.submit(request(vec![PathBuf::from("../escape.wav")])),
            Err(CatalogSubmitError::InvalidWavMetadataRequest(
                CatalogWavMetadataRequestError::InvalidPath {
                    reason: wavecrate_library::sample_sources::reconciliation::RootRelativePathError::ParentTraversal,
                    ..
                }
            ))
        ));
        let oversized = (0..=MAX_WAV_METADATA_REQUEST_PATHS)
            .map(|index| PathBuf::from(format!("{index}.wav")))
            .collect();
        assert_eq!(
            service.submit(request(oversized)),
            Err(CatalogSubmitError::InvalidWavMetadataRequest(
                CatalogWavMetadataRequestError::TooManyPaths {
                    max: MAX_WAV_METADATA_REQUEST_PATHS,
                    actual: MAX_WAV_METADATA_REQUEST_PATHS + 1,
                }
            ))
        );
        service.shutdown();
    }

    #[test]
    fn removal_fences_queued_metadata_completion() {
        let fixture = Fixture::new("catalog-wav-metadata-remove");
        let path = fixture.root.path().join("removed.wav");
        write_test_wav(&path, 8_000, 3);

        let source_id = SourceId::from_string("source-wav-metadata-remove");
        let service = CatalogService::start(&fixture.guard).expect("catalog service");
        let snapshot = register_and_audit(&service, fixture.root.path(), &source_id);
        service
            .submit(CatalogCommand::RequestWavMetadata {
                source_id: source_id.clone(),
                lifecycle_generation: 1,
                expected_revision: snapshot.committed_revision,
                request_id: 45,
                relative_paths: vec![PathBuf::from("removed.wav")],
            })
            .expect("metadata request");
        service
            .submit(CatalogCommand::Remove {
                source_id: source_id.clone(),
                lifecycle_generation: 1,
            })
            .expect("remove request");

        let event = receive_until(&service, |event| {
            matches!(event, CatalogEvent::Removed { .. })
        });
        assert!(matches!(
            event,
            CatalogEvent::Removed {
                source_id: event_source,
                lifecycle_generation: 1,
            } if event_source == source_id
        ));
        assert!(matches!(service.try_recv_event(), Err(TryRecvError::Empty)));
        service.shutdown();
    }

    #[test]
    fn canceled_metadata_token_fences_before_filesystem_read() {
        let fixture = Fixture::new("catalog-wav-metadata-canceled");
        let source_id = SourceId::from_string("source-wav-metadata-canceled");
        let database = SourceDatabase::open_for_catalog_with_profile_guard(
            fixture.root.path(),
            &fixture.guard,
            catalog_profile_child(&source_id),
        )
        .expect("profile catalog database");
        let root_identity = source_root_identity(fixture.root.path()).expect("root identity");
        let token = Arc::new(AtomicBool::new(true));
        let admission = Arc::new(Mutex::new(AdmissionState::new()));
        admission.lock().expect("admission lock").sources.insert(
            source_id.clone(),
            DesiredSource {
                lifecycle_generation: 1,
                active: true,
                attempt_token: Some(Arc::clone(&token)),
                last_curation_request_id: None,
            },
        );
        let source = RegisteredSource {
            root: fixture.root.path().to_path_buf(),
            lifecycle_generation: 1,
            database,
            known_root_identity: Some(root_identity.clone()),
            snapshot: Some(CatalogSnapshot {
                source_id: source_id.clone(),
                root_identity,
                lifecycle_generation: 1,
                committed_revision: 0,
                directory_generation: 0,
                manifest: Vec::new(),
                directories: Vec::new(),
                wav_metadata: BTreeMap::new(),
                curation_metadata: BTreeMap::new(),
            }),
            attempt_token: Some(Arc::clone(&token)),
            curation_lineage: None,
        };

        assert!(matches!(
            wav_metadata::read_patch(
                &source,
                &source_id,
                1,
                0,
                vec![PathBuf::from("canceled.wav")],
                &token,
                &admission,
                &fixture.guard,
            ),
            Err(CatalogError::Canceled)
        ));
    }

    #[test]
    fn audited_snapshot_includes_descriptor_checked_wav_metadata() {
        let fixture = Fixture::new("catalog-wav-metadata");
        let path = fixture.root.path().join("stereo.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).expect("create WAV");
        for frame in 0..7_i16 {
            writer.write_sample(frame).expect("left sample");
            writer.write_sample(-frame).expect("right sample");
        }
        writer.finalize().expect("finalize WAV");

        let service = CatalogService::start(&fixture.guard).expect("catalog service");
        let source_id = SourceId::from_string("source-wav-metadata");
        let snapshot = register_and_audit(&service, fixture.root.path(), &source_id);
        assert_eq!(
            snapshot.wav_metadata.get(Path::new("stereo.wav")),
            Some(&CatalogWavMetadata {
                frames: 7,
                sample_rate: 44_100,
            })
        );
        service.shutdown();
    }

    #[test]
    fn registered_snapshot_rehydrates_descriptor_checked_wav_metadata() {
        let fixture = Fixture::new("catalog-wav-metadata-reopen");
        let path = fixture.root.path().join("reopen.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).expect("create WAV");
        for sample in 0..5_i16 {
            writer.write_sample(sample).expect("sample");
        }
        writer.finalize().expect("finalize WAV");

        let source_id = SourceId::from_string("source-wav-metadata-reopen");
        let service = CatalogService::start(&fixture.guard).expect("catalog service");
        let _ = register_and_audit(&service, fixture.root.path(), &source_id);
        service.shutdown();

        let service = CatalogService::start(&fixture.guard).expect("reopened catalog service");
        service
            .submit(CatalogCommand::Register {
                source_id: source_id.clone(),
                canonical_root: fixture.root.path().to_path_buf(),
                lifecycle_generation: 2,
            })
            .expect("register reopen");
        let event = receive_until(&service, |event| {
            matches!(event, CatalogEvent::SnapshotReady { .. })
        });
        let CatalogEvent::SnapshotReady { snapshot } = event else {
            unreachable!();
        };
        assert_eq!(
            snapshot.wav_metadata.get(Path::new("reopen.wav")),
            Some(&CatalogWavMetadata {
                frames: 5,
                sample_rate: 16_000,
            })
        );
        service.shutdown();
    }

    #[test]
    fn direct_profile_catalog_factory_smoke() {
        let fixture = Fixture::new("catalog-factory");
        let child = PathBuf::from("catalog-smoke");
        fixture
            .guard
            .open_child_dir(Path::new("catalog-direct"))
            .expect("profile child directory");
        let first = SourceDatabase::open_for_catalog_with_profile_guard(
            fixture.root.path(),
            &fixture.guard,
            &child,
        )
        .expect("profile catalog database");
        let duplicate = match SourceDatabase::open_for_catalog_with_profile_guard(
            fixture.root.path(),
            &fixture.guard,
            &child,
        ) {
            Ok(_) => panic!("duplicate profile child lease was accepted"),
            Err(error) => error,
        };
        assert!(matches!(
            duplicate,
            SourceDbError::ProfileOwnership(
                ProfileOwnershipError::ProfileChildAlreadyLeased { .. }
            )
        ));
        drop(first);
    }

    #[test]
    fn audit_uses_profile_catalog_without_source_local_database() {
        let fixture = Fixture::new("catalog-audit");
        let service = CatalogService::start(&fixture.guard).expect("catalog service");
        let source_id = SourceId::from_string("source-a");
        let snapshot = register_and_audit(&service, fixture.root.path(), &source_id);
        assert_eq!(snapshot.manifest, Vec::<SourceManifestEntry>::new());
        assert!(snapshot.directories.is_empty());
        assert!(!fixture.root.path().join(".wavecrate.db").exists());
        service
            .submit(CatalogCommand::ReadSnapshot {
                source_id,
                expected_revision: Some(snapshot.committed_revision),
            })
            .expect("read snapshot after audit");
        assert!(matches!(
            receive_until(&service, |event| {
                matches!(event, CatalogEvent::SnapshotReady { .. })
            }),
            CatalogEvent::SnapshotReady { .. }
        ));
        service.shutdown();
    }

    #[test]
    fn audit_snapshot_reopens_with_revision_and_empty_directory_truth() {
        let fixture = Fixture::new("catalog-reopen");
        let source_id = SourceId::from_string("source-a");
        let first = {
            let service = CatalogService::start(&fixture.guard).expect("catalog service");
            let first = register_and_audit(&service, fixture.root.path(), &source_id);
            service.shutdown();
            first
        };
        let service = CatalogService::start(&fixture.guard).expect("catalog service reopen");
        service
            .submit(CatalogCommand::Register {
                source_id: source_id.clone(),
                canonical_root: fixture.root.path().to_path_buf(),
                lifecycle_generation: 2,
            })
            .expect("register reopen");
        let reopened = receive_until(&service, |event| {
            matches!(event, CatalogEvent::SnapshotReady { .. })
        });
        let CatalogEvent::SnapshotReady { snapshot } = reopened else {
            unreachable!();
        };
        assert_eq!(snapshot.committed_revision, first.committed_revision);
        assert_eq!(snapshot.directories, first.directories);
        assert!(snapshot.directories.is_empty());
        service.shutdown();
    }

    #[test]
    fn audit_publishes_complete_directory_truth_at_exact_manifest_revision() {
        let fixture = Fixture::new("catalog-directory-truth");
        let nested = fixture.root.path().join("drums").join("kicks");
        fs::create_dir_all(&nested).expect("nested source directories");
        let source_id = SourceId::from_string("source-a");
        let service = CatalogService::start(&fixture.guard).expect("catalog service");
        let snapshot = register_and_audit(&service, fixture.root.path(), &source_id);

        assert_eq!(snapshot.committed_revision, 1);
        assert_eq!(snapshot.directories.len(), 2);
        assert_eq!(
            snapshot
                .directories
                .iter()
                .map(|entry| entry.relative_path.clone())
                .collect::<Vec<_>>(),
            vec![PathBuf::from("drums"), PathBuf::from("drums/kicks")]
        );
        service.shutdown();
    }

    #[test]
    fn replacing_registered_generation_releases_child_lease() {
        let fixture = Fixture::new("catalog-generation-replacement");
        let source_id = SourceId::from_string("source-a");
        let service = CatalogService::start(&fixture.guard).expect("catalog service");
        let _ = register_and_audit(&service, fixture.root.path(), &source_id);
        service
            .submit(CatalogCommand::Register {
                source_id: source_id.clone(),
                canonical_root: fixture.root.path().to_path_buf(),
                lifecycle_generation: 2,
            })
            .expect("replacement register");
        let registered = receive_until(&service, |event| {
            matches!(
                event,
                CatalogEvent::Registered {
                    lifecycle_generation: 2,
                    ..
                }
            )
        });
        assert!(matches!(
            registered,
            CatalogEvent::Registered {
                lifecycle_generation: 2,
                ..
            }
        ));
        service
            .submit(CatalogCommand::RequestAudit {
                source_id,
                lifecycle_generation: 2,
            })
            .expect("replacement audit");
        let event = receive_until(&service, |event| {
            matches!(
                event,
                CatalogEvent::AuditCommitted {
                    snapshot: CatalogSnapshot {
                        lifecycle_generation: 2,
                        ..
                    },
                    ..
                }
            )
        });
        assert!(matches!(event, CatalogEvent::AuditCommitted { .. }));
        service.shutdown();
    }

    #[cfg(unix)]
    #[test]
    fn replacing_retained_child_path_fails_closed_without_touching_replacement() {
        let fixture = Fixture::new("catalog-child-replacement");
        let service = CatalogService::start(&fixture.guard).expect("catalog service");
        let source_id = SourceId::from_string("source-a");
        service
            .submit(CatalogCommand::Register {
                source_id: source_id.clone(),
                canonical_root: fixture.root.path().to_path_buf(),
                lifecycle_generation: 1,
            })
            .expect("register");
        let _ = receive_until(&service, |event| {
            matches!(event, CatalogEvent::Registered { .. })
        });

        let child_path = fixture
            .guard
            .profile_root()
            .join(catalog_profile_child(&source_id));
        let moved_path = fixture.guard.profile_root().join("catalog-moved-child");
        fs::rename(&child_path, &moved_path).expect("rename retained child");
        fs::create_dir(&child_path).expect("replacement child");

        service
            .submit(CatalogCommand::RequestAudit {
                source_id,
                lifecycle_generation: 1,
            })
            .expect("audit");
        let event = receive_until(&service, |event| {
            matches!(
                event,
                CatalogEvent::Failed {
                    operation: CatalogOperation::Audit,
                    error: CatalogError::ProfileUnavailable(_),
                    ..
                }
            )
        });
        assert!(matches!(
            event,
            CatalogEvent::Failed {
                operation: CatalogOperation::Audit,
                error: CatalogError::ProfileUnavailable(_),
                ..
            }
        ));
        assert!(
            fs::read_dir(&child_path)
                .expect("replacement child remains readable")
                .next()
                .is_none(),
            "a failed retained-child audit must not write the replacement directory"
        );
        service.shutdown();
    }

    #[cfg(unix)]
    #[test]
    fn canceled_cleanup_revalidates_retained_child_before_writing() {
        let fixture = Fixture::new("catalog-cleanup-child-replacement");
        let source_id = SourceId::from_string("source-a");
        let child = catalog_profile_child(&source_id);
        let database = SourceDatabase::open_for_catalog_with_profile_guard(
            fixture.root.path(),
            &fixture.guard,
            &child,
        )
        .expect("profile catalog database");
        let cancel = AtomicBool::new(true);
        let writer = CatalogScanWriter {
            profile_guard: &fixture.guard,
            database: &database,
            cancel: Some(&cancel),
        };
        assert!(writer.try_lock_cleanup().is_ok());

        let child_path = fixture.guard.profile_root().join(&child);
        let moved_path = fixture
            .guard
            .profile_root()
            .join("catalog-cleanup-moved-child");
        fs::rename(&child_path, &moved_path).expect("rename retained child");
        fs::create_dir(&child_path).expect("replacement child");
        fs::write(child_path.join("sentinel"), b"untouched").expect("replacement sentinel");

        assert!(matches!(
            writer.try_lock_cleanup(),
            Err(ScanError::Io { .. })
        ));
        assert!(child_path.join("sentinel").is_file());
    }

    #[test]
    fn registration_reclaims_restart_abandoned_staging() {
        let fixture = Fixture::new("catalog-restart-orphan");
        let source_id = SourceId::from_string("source-a");
        let child = catalog_profile_child(&source_id);
        {
            let database = SourceDatabase::open_for_catalog_with_profile_guard(
                fixture.root.path(),
                &fixture.guard,
                &child,
            )
            .expect("profile catalog database");
            database
                .begin_source_directory_truth_generation(7, 1)
                .expect("orphan staging generation");
            database
                .stage_source_directory_truth_entries(
                    7,
                    &[SourceDirectoryEntry {
                        relative_path: PathBuf::from("abandoned"),
                        directory_identity: String::from("abandoned-id"),
                    }],
                )
                .expect("orphan staging row");
        }

        let service = CatalogService::start(&fixture.guard).expect("catalog service");
        service
            .submit(CatalogCommand::Register {
                source_id: source_id.clone(),
                canonical_root: fixture.root.path().to_path_buf(),
                lifecycle_generation: 1,
            })
            .expect("register");
        assert!(matches!(
            receive_until(&service, |event| {
                matches!(event, CatalogEvent::Registered { .. })
            }),
            CatalogEvent::Registered { .. }
        ));
        service
            .submit(CatalogCommand::Remove {
                source_id: source_id.clone(),
                lifecycle_generation: 1,
            })
            .expect("remove");
        assert!(matches!(
            receive_until(&service, |event| {
                matches!(event, CatalogEvent::Removed { .. })
            }),
            CatalogEvent::Removed { .. }
        ));
        service.shutdown();

        let database = SourceDatabase::open_for_catalog_with_profile_guard(
            fixture.root.path(),
            &fixture.guard,
            &child,
        )
        .expect("reopen profile catalog database");
        assert!(
            database
                .staged_source_directory_truth_generations(SOURCE_DIRECTORY_TRUTH_BATCH_LIMIT)
                .expect("staging generations")
                .is_empty()
        );
        assert_eq!(database.get_revision().expect("source revision"), 0);
    }

    #[test]
    fn successful_audits_retain_only_one_inactive_directory_generation() {
        let fixture = Fixture::new("catalog-generation-retention");
        let source_id = SourceId::from_string("source-a");
        let service = CatalogService::start(&fixture.guard).expect("catalog service");
        let _ = register_and_audit(&service, fixture.root.path(), &source_id);
        for _ in 0..2 {
            service
                .submit(CatalogCommand::RequestAudit {
                    source_id: source_id.clone(),
                    lifecycle_generation: 1,
                })
                .expect("audit");
            assert!(matches!(
                receive_until(&service, |event| {
                    matches!(event, CatalogEvent::AuditCommitted { .. })
                }),
                CatalogEvent::AuditCommitted { .. }
            ));
        }
        service.shutdown();

        let database = SourceDatabase::open_for_catalog_with_profile_guard(
            fixture.root.path(),
            &fixture.guard,
            catalog_profile_child(&source_id),
        )
        .expect("reopen profile catalog database");
        assert!(
            database
                .staged_source_directory_truth_generations(SOURCE_DIRECTORY_TRUTH_BATCH_LIMIT)
                .expect("staging generations")
                .is_empty()
        );
        assert_eq!(database.get_revision().expect("source revision"), 3);
        let cleanup = database
            .cleanup_source_directory_truth(1)
            .expect("inactive generation cleanup");
        assert_eq!(cleanup.deleted_generations, 1);
        assert!(!cleanup.more_work);
    }

    #[cfg(unix)]
    #[test]
    fn maintenance_fails_closed_after_retained_child_replacement() {
        let fixture = Fixture::new("catalog-maintenance-child-replacement");
        let source_id = SourceId::from_string("source-a");
        let child = catalog_profile_child(&source_id);
        let database = SourceDatabase::open_for_catalog_with_profile_guard(
            fixture.root.path(),
            &fixture.guard,
            &child,
        )
        .expect("profile catalog database");
        database
            .begin_source_directory_truth_generation(1, 1)
            .expect("staging generation");
        database
            .stage_source_directory_truth_entries(
                1,
                &[SourceDirectoryEntry {
                    relative_path: PathBuf::from("pending"),
                    directory_identity: String::from("pending-id"),
                }],
            )
            .expect("staging row");

        let child_path = fixture.guard.profile_root().join(&child);
        let moved_path = fixture
            .guard
            .profile_root()
            .join("catalog-maintenance-moved-child");
        fs::rename(&child_path, &moved_path).expect("rename retained child");
        fs::create_dir(&child_path).expect("replacement child");
        fs::write(child_path.join("sentinel"), b"untouched").expect("replacement sentinel");

        assert!(matches!(
            maintain_catalog_database(&database, &fixture.guard, None, || false),
            Err(CatalogError::ProfileUnavailable(_))
        ));
        assert!(child_path.join("sentinel").is_file());
        assert_eq!(
            database
                .staged_source_directory_truth_generations(SOURCE_DIRECTORY_TRUTH_BATCH_LIMIT)
                .expect("retained staging generation"),
            vec![1]
        );
    }

    #[test]
    fn canceled_registration_maintenance_stops_between_batches_for_later_recovery() {
        let fixture = Fixture::new("catalog-registration-maintenance-cancel");
        let source_id = SourceId::from_string("source-a");
        let database = SourceDatabase::open_for_catalog_with_profile_guard(
            fixture.root.path(),
            &fixture.guard,
            catalog_profile_child(&source_id),
        )
        .expect("profile catalog database");
        let entry_count = SOURCE_DIRECTORY_TRUTH_BATCH_LIMIT * 2 + 1;
        database
            .begin_source_directory_truth_generation(1, entry_count as u64)
            .expect("staging generation");
        let entries = (0..entry_count)
            .map(|index| SourceDirectoryEntry {
                relative_path: PathBuf::from(format!("pending/{index}")),
                directory_identity: format!("pending-{index}"),
            })
            .collect::<Vec<_>>();
        for chunk in entries.chunks(SOURCE_DIRECTORY_TRUTH_BATCH_LIMIT) {
            database
                .stage_source_directory_truth_entries(1, chunk)
                .expect("staging rows");
        }

        let admission = Arc::new(Mutex::new(AdmissionState::new()));
        admission.lock().expect("admission lock").sources.insert(
            source_id.clone(),
            DesiredSource {
                lifecycle_generation: 1,
                active: true,
                attempt_token: None,
                last_curation_request_id: None,
            },
        );
        let predicate_admission = Arc::clone(&admission);
        let predicate_source_id = source_id.clone();
        let checks = AtomicUsize::new(0);
        let result = maintain_catalog_database(&database, &fixture.guard, None, || {
            if checks.fetch_add(1, Ordering::SeqCst) == 2 {
                predicate_admission
                    .lock()
                    .expect("admission lock")
                    .sources
                    .get_mut(&predicate_source_id)
                    .expect("registered source")
                    .active = false;
            }
            !publication_is_current(&predicate_admission, &predicate_source_id, 1, None)
        });
        assert!(matches!(result, Err(CatalogError::Canceled)));
        assert_eq!(checks.load(Ordering::SeqCst), 3);
        assert_eq!(database.get_revision().expect("source revision"), 0);
        assert_eq!(
            database
                .source_directory_truth_state()
                .expect("directory truth state"),
            SourceDirectoryTruthState::Uninitialized
        );
        assert_eq!(
            database
                .staged_source_directory_truth_generations(SOURCE_DIRECTORY_TRUTH_BATCH_LIMIT)
                .expect("remaining staging generation"),
            vec![1]
        );

        maintain_catalog_database(&database, &fixture.guard, None, || false)
            .expect("later registration recovery");
        assert!(
            database
                .staged_source_directory_truth_generations(SOURCE_DIRECTORY_TRUTH_BATCH_LIMIT)
                .expect("recovered staging generations")
                .is_empty()
        );
    }

    #[test]
    fn stale_generation_does_not_publish_audit_result() {
        let fixture = Fixture::new("catalog-stale-generation");
        let service = CatalogService::start(&fixture.guard).expect("catalog service");
        let source_id = SourceId::from_string("source-a");
        service
            .submit(CatalogCommand::Register {
                source_id: source_id.clone(),
                canonical_root: fixture.root.path().to_path_buf(),
                lifecycle_generation: 2,
            })
            .expect("register");
        let _ = receive_until(&service, |event| {
            matches!(event, CatalogEvent::Registered { .. })
        });
        service
            .submit(CatalogCommand::RequestAudit {
                source_id: source_id.clone(),
                lifecycle_generation: 1,
            })
            .expect("stale audit admission");
        thread::sleep(Duration::from_millis(50));
        assert!(matches!(service.try_recv_event(), Err(TryRecvError::Empty)));
        service.shutdown();
    }

    #[test]
    fn accepted_remove_before_register_start_cancels_queued_generation() {
        let fixture = Fixture::new("catalog-remove-before-start");
        let service = CatalogService::start(&fixture.guard).expect("catalog service");
        let source_id = SourceId::from_string("source-a");
        service
            .submit(CatalogCommand::Register {
                source_id: source_id.clone(),
                canonical_root: fixture.root.path().to_path_buf(),
                lifecycle_generation: 1,
            })
            .expect("register");
        service
            .submit(CatalogCommand::Remove {
                source_id,
                lifecycle_generation: 1,
            })
            .expect("remove admission");
        assert!(matches!(
            receive_until(&service, |event| {
                matches!(event, CatalogEvent::Removed { .. })
            }),
            CatalogEvent::Removed { .. }
        ));
        assert!(matches!(service.try_recv_event(), Err(TryRecvError::Empty)));
        service.shutdown();
    }

    #[test]
    fn full_admission_does_not_invalidate_existing_attempt() {
        let fixture = Fixture::new("catalog-full-admission");
        let service = CatalogService::start(&fixture.guard).expect("catalog service");
        let source_id = SourceId::from_string("source-a");
        let token = Arc::new(AtomicBool::new(false));
        {
            let mut admission = service.admission.lock().expect("admission lock");
            admission.event_budget = EVENT_CAPACITY;
            admission.sources.insert(
                source_id.clone(),
                DesiredSource {
                    lifecycle_generation: 1,
                    active: true,
                    attempt_token: Some(Arc::clone(&token)),
                    last_curation_request_id: None,
                },
            );
        }
        let result = service.submit(CatalogCommand::Register {
            source_id,
            canonical_root: fixture.root.path().to_path_buf(),
            lifecycle_generation: 2,
        });
        assert_eq!(result, Err(CatalogSubmitError::Full));
        assert!(!token.load(Ordering::Acquire));
        service.shutdown();
    }

    #[test]
    fn shutdown_cancels_audit_and_closes_without_event_consumer() {
        let fixture = Fixture::new("catalog-shutdown");
        let service = CatalogService::start(&fixture.guard).expect("catalog service");
        let source_id = SourceId::from_string("source-a");
        service
            .submit(CatalogCommand::Register {
                source_id: source_id.clone(),
                canonical_root: fixture.root.path().to_path_buf(),
                lifecycle_generation: 1,
            })
            .expect("register");
        let _ = receive_until(&service, |event| {
            matches!(event, CatalogEvent::Registered { .. })
        });
        service
            .submit(CatalogCommand::RequestAudit {
                source_id,
                lifecycle_generation: 1,
            })
            .expect("audit");
        thread::sleep(Duration::from_millis(1));
        service.shutdown();
    }

    #[test]
    fn shutdown_before_queued_audit_starts_does_not_publish_partial_completion() {
        let fixture = Fixture::new("catalog-shutdown-before-start");
        let service = CatalogService::start(&fixture.guard).expect("catalog service");
        let source_id = SourceId::from_string("source-a");
        service
            .submit(CatalogCommand::Register {
                source_id: source_id.clone(),
                canonical_root: fixture.root.path().to_path_buf(),
                lifecycle_generation: 1,
            })
            .expect("register");
        service
            .submit(CatalogCommand::RequestAudit {
                source_id,
                lifecycle_generation: 1,
            })
            .expect("audit");
        service.shutdown();
    }

    #[test]
    fn profile_guard_is_shared_without_second_profile_lock() {
        let fixture = Fixture::new("catalog-profile-conflict");
        let service = CatalogService::start(&fixture.guard).expect("catalog service");
        assert!(matches!(
            WritableProfileGuard::acquire_current(),
            Err(ProfileOwnershipError::ProfileOwnedByAnotherProcess { .. })
        ));
        service.shutdown();
        drop(fixture.guard);
        let replacement = WritableProfileGuard::acquire_current().expect("released profile");
        drop(replacement);
    }

    #[test]
    fn curation_commit_returns_receipt_and_updates_cached_metadata_projection() {
        let fixture = Fixture::new("catalog-curation-commit");
        write_test_wav(&fixture.root.path().join("one.wav"), 16_000, 4);
        let source_id = SourceId::from_string("source-curation-commit");
        let service = CatalogService::start(&fixture.guard).expect("catalog service");
        let snapshot = register_and_audit(&service, fixture.root.path(), &source_id);
        let path = RootRelativePath::try_from_path(PathBuf::from("one.wav"))
            .expect("relative curation path");
        let collection =
            wavecrate_library::sample_sources::SampleCollection::new(0).expect("collection slot");
        service
            .submit(CatalogCommand::ApplyCuration {
                source_id: source_id.clone(),
                lifecycle_generation: 1,
                expected_revision: snapshot.committed_revision,
                root_identity: snapshot.root_identity.clone(),
                request_id: 501,
                updates: vec![
                    SourceCurationUpdate::SetRating {
                        path: path.clone(),
                        rating: wavecrate_library::sample_sources::Rating::KEEP_3,
                    },
                    SourceCurationUpdate::SetLocked {
                        path: path.clone(),
                        locked: true,
                    },
                    SourceCurationUpdate::AddCollection { path, collection },
                ],
            })
            .expect("curation admission");

        let event = receive_until(&service, |event| {
            matches!(
                event,
                CatalogEvent::CurationCommitted {
                    request_id: 501,
                    ..
                }
            )
        });
        let CatalogEvent::CurationCommitted { receipt, .. } = event else {
            unreachable!();
        };
        assert_eq!(receipt.source_revision(), snapshot.committed_revision + 1);
        assert_eq!(receipt.root_identity(), &snapshot.root_identity);
        assert_eq!(
            receipt.directory_generation(),
            snapshot.directory_generation
        );
        assert_eq!(receipt.files().len(), 1);
        assert_eq!(receipt.files()[0].rating.val(), 3);
        assert!(receipt.files()[0].locked);
        assert_eq!(receipt.files()[0].collections, vec![collection]);
        let committed_revision = receipt.source_revision();

        service
            .submit(CatalogCommand::ReadSnapshot {
                source_id,
                expected_revision: Some(committed_revision),
            })
            .expect("read patched snapshot");
        let event = receive_until(&service, |event| {
            matches!(event, CatalogEvent::SnapshotReady { .. })
        });
        let CatalogEvent::SnapshotReady { snapshot } = event else {
            unreachable!();
        };
        let metadata = snapshot
            .curation_metadata
            .get(Path::new("one.wav"))
            .expect("curation metadata row");
        assert_eq!(metadata.rating.val(), 3);
        assert!(metadata.locked);
        assert_eq!(metadata.collections, vec![collection]);
        assert_eq!(snapshot.committed_revision, committed_revision);
        service.shutdown();
    }

    #[test]
    fn curation_tag_admission_validates_labels_and_releases_payload_credit() {
        let fixture = Fixture::new("catalog-curation-tags");
        write_test_wav(&fixture.root.path().join("one.wav"), 16_000, 4);
        let source_id = SourceId::from_string("source-curation-tags");
        let (service, test_gate) =
            CatalogService::start_with_test_gate(&fixture.guard).expect("catalog service");
        let snapshot = register_and_audit(&service, fixture.root.path(), &source_id);
        let path = RootRelativePath::try_from_path(PathBuf::from("one.wav"))
            .expect("relative curation path");

        let submit = |label: String| {
            service.submit(CatalogCommand::ApplyCuration {
                source_id: source_id.clone(),
                lifecycle_generation: 1,
                expected_revision: snapshot.committed_revision,
                root_identity: snapshot.root_identity.clone(),
                request_id: 601,
                updates: vec![SourceCurationUpdate::AddTag {
                    path: path.clone(),
                    label,
                }],
            })
        };
        assert!(matches!(
            submit("x".repeat(MAX_CURATION_TAG_LABEL_BYTES + 1)),
            Err(CatalogSubmitError::InvalidCurationRequest(
                CatalogCurationRequestError::TagLabelTooLong { .. }
            ))
        ));
        assert!(matches!(
            submit(String::from("bad\0tag")),
            Err(CatalogSubmitError::InvalidCurationRequest(
                CatalogCurationRequestError::TagLabelContainsNul
            ))
        ));
        assert!(matches!(
            submit(String::from(" \t\n")),
            Err(CatalogSubmitError::InvalidCurationRequest(
                CatalogCurationRequestError::EmptyTagLabel
            ))
        ));
        let baseline_event_budget = service
            .admission
            .lock()
            .expect("admission lock")
            .event_budget;
        assert_eq!(
            service
                .admission
                .lock()
                .expect("admission lock")
                .curation_payload_bytes,
            0
        );

        let pause = test_gate.pause_next();
        let valid_label = String::from(" LOOP ");
        let valid_update = SourceCurationUpdate::AddTag {
            path,
            label: valid_label,
        };
        service
            .submit(CatalogCommand::ApplyCuration {
                source_id: source_id.clone(),
                lifecycle_generation: 1,
                expected_revision: snapshot.committed_revision,
                root_identity: snapshot.root_identity.clone(),
                request_id: 602,
                updates: vec![valid_update.clone()],
            })
            .expect("tag curation admission");
        assert_eq!(
            service
                .admission
                .lock()
                .expect("admission lock")
                .curation_payload_bytes,
            curation::operation_payload_bytes(
                &snapshot.root_identity,
                std::slice::from_ref(&vec![valid_update])
            )
        );
        pause.release();
        let event = receive_until(&service, |event| {
            matches!(
                event,
                CatalogEvent::CurationCommitted {
                    request_id: 602,
                    ..
                }
            )
        });
        let CatalogEvent::CurationCommitted { receipt, .. } = event else {
            unreachable!();
        };
        assert_eq!(
            receipt.files()[0].playback_type,
            Some(wavecrate_library::sample_sources::SourcePlaybackType::Loop)
        );
        assert_eq!(
            service
                .admission
                .lock()
                .expect("admission lock")
                .curation_payload_bytes,
            0
        );
        assert_eq!(
            service
                .admission
                .lock()
                .expect("admission lock")
                .event_budget,
            baseline_event_budget
        );
        service.shutdown();
    }

    #[test]
    fn curation_operation_label_bytes_can_exceed_the_bounded_payload() {
        let fixture = Fixture::new("catalog-curation-tag-payload");
        write_test_wav(&fixture.root.path().join("one.wav"), 16_000, 4);
        let source_id = SourceId::from_string("source-curation-tag-payload");
        let service = CatalogService::start(&fixture.guard).expect("catalog service");
        let snapshot = register_and_audit(&service, fixture.root.path(), &source_id);
        let path_len = MAX_CURATION_OPERATION_PATH_BYTES - 640;
        let path = RootRelativePath::try_from_path(PathBuf::from("x".repeat(path_len)))
            .expect("large relative curation path");
        let result = service.submit(curation_operation(
            &source_id,
            1,
            &snapshot.root_identity,
            603,
            SourceCurationOperationFence::ExactRevision(snapshot.committed_revision),
            vec![vec![SourceCurationUpdate::AddTag {
                path,
                label: "x".repeat(MAX_CURATION_TAG_LABEL_BYTES),
            }]],
        ));
        assert!(matches!(
            result,
            Err(CatalogSubmitError::InvalidCurationRequest(
                CatalogCurationRequestError::OperationPayloadTooLarge {
                    max: MAX_CURATION_OPERATION_RECEIPT_BYTES,
                    actual,
                }
            )) if actual > MAX_CURATION_OPERATION_RECEIPT_BYTES
        ));
        assert_eq!(
            service
                .admission
                .lock()
                .expect("admission lock")
                .curation_payload_bytes,
            0
        );
        service.shutdown();
    }

    #[test]
    fn curation_before_remove_commits_and_remove_admission_fences_later_writes() {
        let fixture = Fixture::new("catalog-curation-remove");
        write_test_wav(&fixture.root.path().join("one.wav"), 16_000, 4);
        let source_id = SourceId::from_string("source-curation-remove");
        let (service, test_gate) =
            CatalogService::start_with_test_gate(&fixture.guard).expect("catalog service");
        let snapshot = register_and_audit(&service, fixture.root.path(), &source_id);
        let update = SourceCurationUpdate::SetRating {
            path: RootRelativePath::try_from_path(PathBuf::from("one.wav"))
                .expect("relative curation path"),
            rating: wavecrate_library::sample_sources::Rating::KEEP_1,
        };
        let pause = test_gate.pause_next();
        service
            .submit(CatalogCommand::ApplyCuration {
                source_id: source_id.clone(),
                lifecycle_generation: 1,
                expected_revision: snapshot.committed_revision,
                root_identity: snapshot.root_identity.clone(),
                request_id: 502,
                updates: vec![update.clone()],
            })
            .expect("curation admission");
        service
            .submit(CatalogCommand::Remove {
                source_id: source_id.clone(),
                lifecycle_generation: 1,
            })
            .expect("remove admission");
        assert!(matches!(
            service.submit(CatalogCommand::ApplyCuration {
                source_id: source_id.clone(),
                lifecycle_generation: 1,
                expected_revision: snapshot.committed_revision,
                root_identity: snapshot.root_identity.clone(),
                request_id: 503,
                updates: vec![update],
            }),
            Err(CatalogSubmitError::InvalidCurationRequest(
                CatalogCurationRequestError::Inactive { .. }
            ))
        ));
        pause.release();

        let first = receive_until(&service, |event| {
            matches!(
                event,
                CatalogEvent::CurationCommitted {
                    request_id: 502,
                    ..
                }
            )
        });
        assert!(matches!(first, CatalogEvent::CurationCommitted { .. }));
        assert!(matches!(
            receive_until(&service, |event| {
                matches!(event, CatalogEvent::Removed { .. })
            }),
            CatalogEvent::Removed { .. }
        ));
        service.shutdown();
        let database = SourceDatabase::open_for_catalog_with_profile_guard(
            fixture.root.path(),
            &fixture.guard,
            catalog_profile_child(&source_id),
        )
        .expect("reopen committed source database");
        let metadata = database
            .browser_metadata_snapshot()
            .expect("read committed curation metadata");
        assert_eq!(metadata.revision, snapshot.committed_revision + 1);
        assert_eq!(metadata.files[0].rating.val(), 1);
    }

    #[test]
    fn curation_before_register_replacement_commits_against_old_binding() {
        let fixture = Fixture::new("catalog-curation-register-replacement");
        write_test_wav(&fixture.root.path().join("one.wav"), 16_000, 4);
        let replacement_root = tempfile::tempdir().expect("replacement source root");
        let source_id = SourceId::from_string("source-curation-register-replacement");
        let (service, test_gate) =
            CatalogService::start_with_test_gate(&fixture.guard).expect("catalog service");
        let snapshot = register_and_audit(&service, fixture.root.path(), &source_id);
        let path = RootRelativePath::try_from_path(PathBuf::from("one.wav"))
            .expect("relative curation path");
        let pause = test_gate.pause_next();
        service
            .submit(CatalogCommand::ApplyCuration {
                source_id: source_id.clone(),
                lifecycle_generation: 1,
                expected_revision: snapshot.committed_revision,
                root_identity: snapshot.root_identity.clone(),
                request_id: 509,
                updates: vec![SourceCurationUpdate::SetRating {
                    path,
                    rating: wavecrate_library::sample_sources::Rating::KEEP_1,
                }],
            })
            .expect("curation admission");
        service
            .submit(CatalogCommand::Register {
                source_id: source_id.clone(),
                canonical_root: replacement_root.path().to_path_buf(),
                lifecycle_generation: 2,
            })
            .expect("replacement registration admission");
        {
            let admission = service.admission.lock().expect("admission lock");
            assert_eq!(
                admission
                    .sources
                    .get(&source_id)
                    .expect("replacement admission")
                    .lifecycle_generation,
                2
            );
        }
        pause.release();

        let event = receive_until(&service, |event| {
            matches!(
                event,
                CatalogEvent::CurationCommitted {
                    request_id: 509,
                    ..
                }
            )
        });
        assert!(matches!(event, CatalogEvent::CurationCommitted { .. }));
        assert!(matches!(
            receive_until(&service, |event| {
                matches!(
                    event,
                    CatalogEvent::Registered {
                        lifecycle_generation: 2,
                        ..
                    }
                )
            }),
            CatalogEvent::Registered {
                lifecycle_generation: 2,
                ..
            }
        ));
        service.shutdown();

        let database = SourceDatabase::open_for_catalog_with_profile_guard(
            replacement_root.path(),
            &fixture.guard,
            catalog_profile_child(&source_id),
        )
        .expect("reopen replacement source database");
        let metadata = database
            .browser_metadata_snapshot()
            .expect("read old binding curation metadata");
        assert_eq!(metadata.revision, snapshot.committed_revision + 1);
        assert_eq!(metadata.files[0].rating.val(), 1);
    }

    #[test]
    fn curation_is_terminal_snapshot_unavailable_before_first_audit() {
        let fixture = Fixture::new("catalog-curation-pre-snapshot");
        let source_id = SourceId::from_string("source-curation-pre-snapshot");
        let service = CatalogService::start(&fixture.guard).expect("catalog service");
        service
            .submit(CatalogCommand::Register {
                source_id: source_id.clone(),
                canonical_root: fixture.root.path().to_path_buf(),
                lifecycle_generation: 1,
            })
            .expect("register");
        let root_identity = source_root_identity(fixture.root.path()).expect("root identity");
        service
            .submit(CatalogCommand::ApplyCuration {
                source_id: source_id.clone(),
                lifecycle_generation: 1,
                expected_revision: 0,
                root_identity,
                request_id: 504,
                updates: vec![SourceCurationUpdate::SetRating {
                    path: RootRelativePath::try_from_path(PathBuf::from("one.wav"))
                        .expect("relative curation path"),
                    rating: wavecrate_library::sample_sources::Rating::KEEP_1,
                }],
            })
            .expect("pre-snapshot curation admission");

        let event = receive_until(&service, |event| {
            matches!(
                event,
                CatalogEvent::CurationFailed {
                    request_id: 504,
                    ..
                }
            )
        });
        assert!(matches!(
            event,
            CatalogEvent::CurationFailed {
                error: CatalogError::SnapshotUnavailable,
                ..
            }
        ));
        service.shutdown();
    }

    #[test]
    fn curation_same_revision_conflict_has_one_commit_and_one_terminal_stale_failure() {
        let fixture = Fixture::new("catalog-curation-conflict");
        write_test_wav(&fixture.root.path().join("one.wav"), 16_000, 4);
        let source_id = SourceId::from_string("source-curation-conflict");
        let service = CatalogService::start(&fixture.guard).expect("catalog service");
        let snapshot = register_and_audit(&service, fixture.root.path(), &source_id);
        let update = SourceCurationUpdate::SetRating {
            path: RootRelativePath::try_from_path(PathBuf::from("one.wav"))
                .expect("relative curation path"),
            rating: wavecrate_library::sample_sources::Rating::KEEP_1,
        };
        for request_id in [505, 506] {
            service
                .submit(CatalogCommand::ApplyCuration {
                    source_id: source_id.clone(),
                    lifecycle_generation: 1,
                    expected_revision: snapshot.committed_revision,
                    root_identity: snapshot.root_identity.clone(),
                    request_id,
                    updates: vec![update.clone()],
                })
                .expect("curation admission");
        }
        let first = receive_until(&service, |event| {
            matches!(
                event,
                CatalogEvent::CurationCommitted {
                    request_id: 505,
                    ..
                } | CatalogEvent::CurationFailed {
                    request_id: 505,
                    ..
                }
            )
        });
        let second = receive_until(&service, |event| {
            matches!(
                event,
                CatalogEvent::CurationCommitted {
                    request_id: 506,
                    ..
                } | CatalogEvent::CurationFailed {
                    request_id: 506,
                    ..
                }
            )
        });
        assert!(matches!(first, CatalogEvent::CurationCommitted { .. }));
        assert!(matches!(
            second,
            CatalogEvent::CurationFailed {
                error: CatalogError::StaleRevision { .. },
                ..
            }
        ));
        service.shutdown();
    }

    #[cfg(unix)]
    #[test]
    fn curation_root_replacement_fails_before_mutation() {
        let fixture = Fixture::new("catalog-curation-root-replacement");
        write_test_wav(&fixture.root.path().join("one.wav"), 16_000, 4);
        let source_id = SourceId::from_string("source-curation-root-replacement");
        let service = CatalogService::start(&fixture.guard).expect("catalog service");
        let snapshot = register_and_audit(&service, fixture.root.path(), &source_id);
        let old_root = fixture.root.path().to_path_buf();
        let displaced = old_root.with_extension("moved");
        std::fs::rename(&old_root, &displaced).expect("displace source root");
        std::fs::create_dir(&old_root).expect("replace source root");
        service
            .submit(CatalogCommand::ApplyCuration {
                source_id,
                lifecycle_generation: 1,
                expected_revision: snapshot.committed_revision,
                root_identity: snapshot.root_identity,
                request_id: 507,
                updates: vec![SourceCurationUpdate::SetRating {
                    path: RootRelativePath::try_from_path(PathBuf::from("one.wav"))
                        .expect("relative curation path"),
                    rating: wavecrate_library::sample_sources::Rating::KEEP_1,
                }],
            })
            .expect("curation admission");
        let event = receive_until(&service, |event| {
            matches!(
                event,
                CatalogEvent::CurationFailed {
                    request_id: 507,
                    ..
                }
            )
        });
        assert!(matches!(
            event,
            CatalogEvent::CurationFailed {
                error: CatalogError::StaleRootGeneration { .. },
                ..
            }
        ));
        service.shutdown();
    }

    #[test]
    fn shutdown_drains_admitted_curation_before_closed() {
        let fixture = Fixture::new("catalog-curation-shutdown");
        write_test_wav(&fixture.root.path().join("one.wav"), 16_000, 4);
        let source_id = SourceId::from_string("source-curation-shutdown");
        let (service, test_gate) =
            CatalogService::start_with_test_gate(&fixture.guard).expect("catalog service");
        let snapshot = register_and_audit(&service, fixture.root.path(), &source_id);
        let pause = test_gate.pause_next();
        service
            .submit(CatalogCommand::ApplyCuration {
                source_id: source_id.clone(),
                lifecycle_generation: 1,
                expected_revision: snapshot.committed_revision,
                root_identity: snapshot.root_identity,
                request_id: 508,
                updates: vec![SourceCurationUpdate::SetRating {
                    path: RootRelativePath::try_from_path(PathBuf::from("one.wav"))
                        .expect("relative curation path"),
                    rating: wavecrate_library::sample_sources::Rating::KEEP_3,
                }],
            })
            .expect("curation admission");
        let service = Arc::new(service);
        let shutdown_service = Arc::clone(&service);
        let shutdown_thread = thread::spawn(move || shutdown_service.shutdown());
        while !admission_state(&service.admission, |state| state.shutdown) {
            thread::yield_now();
        }
        pause.release();
        shutdown_thread.join().expect("catalog shutdown");

        let mut seen_commit = false;
        let mut seen_closed = false;
        while let Ok(event) = service.try_recv_event() {
            match event {
                CatalogEvent::CurationCommitted {
                    request_id: 508, ..
                } => {
                    assert!(!seen_closed);
                    seen_commit = true;
                }
                CatalogEvent::Closed => {
                    assert!(seen_commit);
                    seen_closed = true;
                }
                _ => {}
            }
        }
        assert!(seen_commit);
        assert!(seen_closed);
        let database = SourceDatabase::open_for_catalog_with_profile_guard(
            fixture.root.path(),
            &fixture.guard,
            catalog_profile_child(&source_id),
        )
        .expect("reopen committed source database");
        assert_eq!(
            database.get_revision().expect("committed source revision"),
            snapshot.committed_revision + 1
        );
    }

    #[test]
    fn shutdown_drains_full_durable_curation_queue_without_a_sentinel() {
        let fixture = Fixture::new("catalog-curation-shutdown-full-queue");
        write_test_wav(&fixture.root.path().join("one.wav"), 16_000, 4);
        let source_id = SourceId::from_string("source-curation-shutdown-full-queue");
        let (service, test_gate) =
            CatalogService::start_with_test_gate(&fixture.guard).expect("catalog service");
        let snapshot = register_and_audit(&service, fixture.root.path(), &source_id);
        let path = RootRelativePath::try_from_path(PathBuf::from("one.wav"))
            .expect("relative curation path");
        let pause = test_gate.pause_next();
        for index in 0..COMMAND_CAPACITY {
            service
                .submit(CatalogCommand::ApplyCuration {
                    source_id: source_id.clone(),
                    lifecycle_generation: 1,
                    expected_revision: snapshot.committed_revision + index as u64,
                    root_identity: snapshot.root_identity.clone(),
                    request_id: 600 + index as u64,
                    updates: vec![SourceCurationUpdate::SetRating {
                        path: path.clone(),
                        rating: wavecrate_library::sample_sources::Rating::KEEP_1,
                    }],
                })
                .expect("admit queued curation");
        }
        assert_eq!(
            service.submit(CatalogCommand::ApplyCuration {
                source_id: source_id.clone(),
                lifecycle_generation: 1,
                expected_revision: snapshot.committed_revision + COMMAND_CAPACITY as u64,
                root_identity: snapshot.root_identity.clone(),
                request_id: 700,
                updates: vec![SourceCurationUpdate::SetRating {
                    path,
                    rating: wavecrate_library::sample_sources::Rating::KEEP_1,
                }],
            }),
            Err(CatalogSubmitError::Full)
        );

        let service = Arc::new(service);
        let shutdown_service = Arc::clone(&service);
        let shutdown_thread = thread::spawn(move || shutdown_service.shutdown());
        while !admission_state(&service.admission, |state| state.shutdown) {
            thread::yield_now();
        }
        pause.release();
        shutdown_thread.join().expect("catalog shutdown");

        let mut committed_revisions = Vec::new();
        let mut seen_closed = false;
        while let Ok(event) = service.try_recv_event() {
            match event {
                CatalogEvent::CurationCommitted { receipt, .. } => {
                    assert!(!seen_closed);
                    committed_revisions.push(receipt.source_revision());
                }
                CatalogEvent::Closed => {
                    assert!(!seen_closed);
                    seen_closed = true;
                }
                event => panic!("unexpected shutdown event: {event:?}"),
            }
        }
        assert!(seen_closed);
        assert_eq!(committed_revisions.len(), COMMAND_CAPACITY);
        assert_eq!(
            committed_revisions,
            (snapshot.committed_revision + 1
                ..=snapshot.committed_revision + COMMAND_CAPACITY as u64)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            service
                .admission
                .lock()
                .expect("admission lock")
                .event_budget,
            0
        );

        let database = SourceDatabase::open_for_catalog_with_profile_guard(
            fixture.root.path(),
            &fixture.guard,
            catalog_profile_child(&source_id),
        )
        .expect("reopen committed source database");
        assert_eq!(
            database.get_revision().expect("committed source revision"),
            snapshot.committed_revision + COMMAND_CAPACITY as u64
        );
    }

    #[test]
    fn curation_operation_commits_serial_batches_and_chains_only_after_full_success() {
        let fixture = Fixture::new("catalog-curation-operation-chain");
        write_test_wav(&fixture.root.path().join("one.wav"), 16_000, 4);
        let source_id = SourceId::from_string("source-curation-operation-chain");
        let service = CatalogService::start(&fixture.guard).expect("catalog service");
        let snapshot = register_and_audit(&service, fixture.root.path(), &source_id);
        let path = RootRelativePath::try_from_path(PathBuf::from("one.wav"))
            .expect("relative curation path");
        let collection =
            wavecrate_library::sample_sources::SampleCollection::new(0).expect("collection slot");

        service
            .submit(curation_operation(
                &source_id,
                1,
                &snapshot.root_identity,
                800,
                SourceCurationOperationFence::ExactRevision(snapshot.committed_revision),
                vec![
                    vec![SourceCurationUpdate::SetLastPlayedAt {
                        path: path.clone(),
                        played_at: Some(1_800_000_000),
                    }],
                    vec![
                        SourceCurationUpdate::SetRating {
                            path: path.clone(),
                            rating: wavecrate_library::sample_sources::Rating::KEEP_1,
                        },
                        SourceCurationUpdate::SetLocked {
                            path: path.clone(),
                            locked: true,
                        },
                        SourceCurationUpdate::AddCollection { path, collection },
                    ],
                ],
            ))
            .expect("compound curation admission");

        let event = receive_until(&service, |event| {
            matches!(
                event,
                CatalogEvent::CurationOperationCompleted {
                    request_id: 800,
                    ..
                }
            )
        });
        let CatalogEvent::CurationOperationCompleted { outcome, .. } = event else {
            unreachable!();
        };
        assert!(outcome.is_success());
        assert_eq!(
            outcome.initial_revision(),
            Some(snapshot.committed_revision)
        );
        assert_eq!(
            outcome.final_revision(),
            Some(snapshot.committed_revision + 2)
        );
        assert_eq!(outcome.committed_batches().len(), 2);
        assert_eq!(
            outcome.committed_batches()[0].source_revision(),
            snapshot.committed_revision + 1
        );
        assert_eq!(
            outcome.committed_batches()[1].source_revision(),
            snapshot.committed_revision + 2
        );

        service
            .submit(curation_operation(
                &source_id,
                1,
                &snapshot.root_identity,
                801,
                SourceCurationOperationFence::AfterOperation { request_id: 800 },
                vec![vec![
                    SourceCurationUpdate::RemoveCollection {
                        path: RootRelativePath::try_from_path(PathBuf::from("one.wav"))
                            .expect("relative curation path"),
                        collection,
                    },
                    SourceCurationUpdate::SetRating {
                        path: RootRelativePath::try_from_path(PathBuf::from("one.wav"))
                            .expect("relative curation path"),
                        rating: wavecrate_library::sample_sources::Rating::KEEP_3,
                    },
                ]],
            ))
            .expect("successor curation admission");
        let event = receive_until(&service, |event| {
            matches!(
                event,
                CatalogEvent::CurationOperationCompleted {
                    request_id: 801,
                    ..
                }
            )
        });
        let CatalogEvent::CurationOperationCompleted { outcome, .. } = event else {
            unreachable!();
        };
        assert!(outcome.is_success());
        assert_eq!(
            outcome.initial_revision(),
            Some(snapshot.committed_revision + 2)
        );
        assert_eq!(
            outcome.final_revision(),
            Some(snapshot.committed_revision + 3)
        );
        assert_eq!(outcome.committed_batches().len(), 1);
        assert!(outcome.committed_batches()[0].files()[0].locked);
        assert_eq!(
            outcome.committed_batches()[0].files()[0].rating,
            wavecrate_library::sample_sources::Rating::KEEP_3
        );
        assert_eq!(
            outcome.committed_batches()[0].files()[0].last_played_at,
            Some(1_800_000_000)
        );
        assert!(
            outcome.committed_batches()[0].files()[0]
                .collections
                .is_empty()
        );
        service.shutdown();
    }

    #[test]
    fn curation_operation_fifo_chain_survives_before_any_event_is_drained() {
        let fixture = Fixture::new("catalog-curation-operation-fifo");
        write_test_wav(&fixture.root.path().join("one.wav"), 16_000, 4);
        let source_id = SourceId::from_string("source-curation-operation-fifo");
        let (service, test_gate) =
            CatalogService::start_with_test_gate(&fixture.guard).expect("catalog service");
        let snapshot = register_and_audit(&service, fixture.root.path(), &source_id);
        let path = RootRelativePath::try_from_path(PathBuf::from("one.wav"))
            .expect("relative curation path");
        let pause = test_gate.pause_next();

        service
            .submit(curation_operation(
                &source_id,
                1,
                &snapshot.root_identity,
                1_100,
                SourceCurationOperationFence::ExactRevision(snapshot.committed_revision),
                vec![vec![SourceCurationUpdate::SetRating {
                    path: path.clone(),
                    rating: wavecrate_library::sample_sources::Rating::KEEP_1,
                }]],
            ))
            .expect("first operation admission");
        service
            .submit(curation_operation(
                &source_id,
                1,
                &snapshot.root_identity,
                1_101,
                SourceCurationOperationFence::AfterOperation { request_id: 1_100 },
                vec![vec![SourceCurationUpdate::SetLocked { path, locked: true }]],
            ))
            .expect("successor operation admission");
        pause.release();

        let first = receive_curation_operation(&service, 1_100);
        assert!(first.is_success());
        assert_eq!(first.initial_revision(), Some(snapshot.committed_revision));
        assert_eq!(
            first.final_revision(),
            Some(snapshot.committed_revision + 1)
        );
        let second = receive_curation_operation(&service, 1_101);
        assert!(second.is_success());
        assert_eq!(
            second.initial_revision(),
            Some(snapshot.committed_revision + 1)
        );
        assert_eq!(
            second.final_revision(),
            Some(snapshot.committed_revision + 2)
        );
        service.shutdown();
    }

    #[test]
    fn curation_operation_partial_failure_breaks_successor_chain_without_rebasing() {
        let fixture = Fixture::new("catalog-curation-operation-partial");
        write_test_wav(&fixture.root.path().join("one.wav"), 16_000, 4);
        let source_id = SourceId::from_string("source-curation-operation-partial");
        let service = CatalogService::start(&fixture.guard).expect("catalog service");
        let snapshot = register_and_audit(&service, fixture.root.path(), &source_id);
        let valid_path = RootRelativePath::try_from_path(PathBuf::from("one.wav"))
            .expect("relative curation path");

        service
            .submit(curation_operation(
                &source_id,
                1,
                &snapshot.root_identity,
                900,
                SourceCurationOperationFence::ExactRevision(snapshot.committed_revision),
                vec![
                    vec![SourceCurationUpdate::SetRating {
                        path: valid_path.clone(),
                        rating: wavecrate_library::sample_sources::Rating::KEEP_1,
                    }],
                    vec![SourceCurationUpdate::SetRating {
                        path: RootRelativePath::try_from_path(PathBuf::from("missing.wav"))
                            .expect("relative missing path"),
                        rating: wavecrate_library::sample_sources::Rating::KEEP_3,
                    }],
                ],
            ))
            .expect("partial operation admission");
        let event = receive_until(&service, |event| {
            matches!(
                event,
                CatalogEvent::CurationOperationCompleted {
                    request_id: 900,
                    ..
                }
            )
        });
        let CatalogEvent::CurationOperationCompleted { outcome, .. } = event else {
            unreachable!();
        };
        assert!(!outcome.is_success());
        assert_eq!(
            outcome.initial_revision(),
            Some(snapshot.committed_revision)
        );
        assert_eq!(
            outcome.final_revision(),
            Some(snapshot.committed_revision + 1)
        );
        assert_eq!(outcome.committed_batches().len(), 1);
        assert_eq!(outcome.failure().expect("failed batch").batch_index(), 1);

        service
            .submit(curation_operation(
                &source_id,
                1,
                &snapshot.root_identity,
                901,
                SourceCurationOperationFence::AfterOperation { request_id: 900 },
                vec![vec![SourceCurationUpdate::SetRating {
                    path: valid_path,
                    rating: wavecrate_library::sample_sources::Rating::KEEP_3,
                }]],
            ))
            .expect("successor admission");
        let event = receive_until(&service, |event| {
            matches!(
                event,
                CatalogEvent::CurationOperationCompleted {
                    request_id: 901,
                    ..
                }
            )
        });
        let CatalogEvent::CurationOperationCompleted { outcome, .. } = event else {
            unreachable!();
        };
        assert!(!outcome.is_success());
        assert_eq!(outcome.initial_revision(), None);
        assert_eq!(outcome.final_revision(), None);
        assert!(outcome.committed_batches().is_empty());
        assert!(matches!(
            outcome.failure().expect("predecessor failure").error(),
            CatalogError::CurationPredecessorUnavailable {
                request_id: 900,
                ..
            }
        ));

        let expected_prefix_revision = snapshot.committed_revision + 1;
        service
            .submit(CatalogCommand::ReadSnapshot {
                source_id,
                expected_revision: Some(expected_prefix_revision),
            })
            .expect("read committed prefix");
        let event = receive_until(&service, |event| {
            matches!(event, CatalogEvent::SnapshotReady { .. })
        });
        let CatalogEvent::SnapshotReady { snapshot } = event else {
            unreachable!();
        };
        assert_eq!(snapshot.committed_revision, expected_prefix_revision);
        assert_eq!(
            snapshot
                .curation_metadata
                .get(Path::new("one.wav"))
                .expect("prefix metadata")
                .rating
                .val(),
            1
        );
        service.shutdown();
    }

    #[test]
    fn curation_operation_rejects_nonlatest_and_unrelated_revision_without_rebasing() {
        let fixture = Fixture::new("catalog-curation-operation-fences");
        write_test_wav(&fixture.root.path().join("one.wav"), 16_000, 4);
        let source_id = SourceId::from_string("source-curation-operation-fences");
        let service = CatalogService::start(&fixture.guard).expect("catalog service");
        let snapshot = register_and_audit(&service, fixture.root.path(), &source_id);
        let path = RootRelativePath::try_from_path(PathBuf::from("one.wav"))
            .expect("relative curation path");

        service
            .submit(curation_operation(
                &source_id,
                1,
                &snapshot.root_identity,
                1_100,
                SourceCurationOperationFence::ExactRevision(snapshot.committed_revision),
                vec![vec![SourceCurationUpdate::SetRating {
                    path: path.clone(),
                    rating: wavecrate_library::sample_sources::Rating::KEEP_1,
                }]],
            ))
            .expect("first operation admission");
        assert!(receive_curation_operation(&service, 1_100).is_success());

        service
            .submit(curation_operation(
                &source_id,
                1,
                &snapshot.root_identity,
                1_101,
                SourceCurationOperationFence::AfterOperation { request_id: 1_100 },
                vec![vec![SourceCurationUpdate::SetLocked {
                    path: path.clone(),
                    locked: true,
                }]],
            ))
            .expect("second operation admission");
        assert!(receive_curation_operation(&service, 1_101).is_success());

        service
            .submit(curation_operation(
                &source_id,
                1,
                &snapshot.root_identity,
                1_102,
                SourceCurationOperationFence::AfterOperation { request_id: 1_100 },
                vec![vec![SourceCurationUpdate::SetRating {
                    path: path.clone(),
                    rating: wavecrate_library::sample_sources::Rating::KEEP_3,
                }]],
            ))
            .expect("nonlatest predecessor admission");
        let nonlatest = receive_curation_operation(&service, 1_102);
        assert!(!nonlatest.is_success());
        assert!(nonlatest.committed_batches().is_empty());
        assert_eq!(nonlatest.initial_revision(), None);
        assert!(matches!(
            nonlatest.failure().expect("nonlatest failure").error(),
            CatalogError::CurationPredecessorUnavailable {
                request_id: 1_100,
                ..
            }
        ));

        let second_revision = snapshot.committed_revision + 2;
        service
            .submit(curation_operation(
                &source_id,
                1,
                &snapshot.root_identity,
                1_103,
                SourceCurationOperationFence::ExactRevision(second_revision),
                vec![vec![SourceCurationUpdate::SetLocked {
                    path: path.clone(),
                    locked: false,
                }]],
            ))
            .expect("new exact operation admission");
        assert!(receive_curation_operation(&service, 1_103).is_success());

        service
            .submit(CatalogCommand::ApplyCuration {
                source_id: source_id.clone(),
                lifecycle_generation: 1,
                expected_revision: snapshot.committed_revision + 3,
                root_identity: snapshot.root_identity.clone(),
                request_id: 1_104,
                updates: vec![SourceCurationUpdate::SetRating {
                    path: path.clone(),
                    rating: wavecrate_library::sample_sources::Rating::KEEP_3,
                }],
            })
            .expect("unrelated curation admission");
        assert!(matches!(
            receive_until(&service, |event| {
                matches!(
                    event,
                    CatalogEvent::CurationCommitted {
                        request_id: 1_104,
                        ..
                    }
                )
            }),
            CatalogEvent::CurationCommitted { .. }
        ));

        service
            .submit(curation_operation(
                &source_id,
                1,
                &snapshot.root_identity,
                1_105,
                SourceCurationOperationFence::AfterOperation { request_id: 1_103 },
                vec![vec![SourceCurationUpdate::SetLocked {
                    path: path.clone(),
                    locked: true,
                }]],
            ))
            .expect("post-independent-writer admission");
        let after_unrelated = receive_curation_operation(&service, 1_105);
        assert!(!after_unrelated.is_success());
        assert!(after_unrelated.committed_batches().is_empty());
        assert!(matches!(
            after_unrelated
                .failure()
                .expect("independent writer failure")
                .error(),
            CatalogError::CurationPredecessorUnavailable {
                request_id: 1_103,
                ..
            }
        ));

        service
            .submit(curation_operation(
                &source_id,
                1,
                &snapshot.root_identity,
                1_106,
                SourceCurationOperationFence::ExactRevision(snapshot.committed_revision),
                vec![vec![SourceCurationUpdate::SetRating {
                    path,
                    rating: wavecrate_library::sample_sources::Rating::KEEP_1,
                }]],
            ))
            .expect("stale exact operation admission");
        let stale = receive_curation_operation(&service, 1_106);
        assert!(!stale.is_success());
        assert_eq!(stale.initial_revision(), Some(snapshot.committed_revision));
        assert_eq!(stale.final_revision(), Some(snapshot.committed_revision));
        assert!(stale.committed_batches().is_empty());
        assert_eq!(
            stale.failure().expect("stale exact failure").batch_index(),
            0
        );
        assert!(matches!(
            stale.failure().expect("stale exact failure").error(),
            CatalogError::StaleRevision {
                expected,
                actual
            } if *expected == snapshot.committed_revision
                && *actual == snapshot.committed_revision + 4
        ));
        service.shutdown();
    }

    #[test]
    fn curation_operation_lifecycle_replacement_fails_without_rebasing() {
        let fixture = Fixture::new("catalog-curation-operation-replacement");
        write_test_wav(&fixture.root.path().join("one.wav"), 16_000, 4);
        let replacement_root = tempfile::tempdir().expect("replacement source root");
        let source_id = SourceId::from_string("source-curation-operation-replacement");
        let service = CatalogService::start(&fixture.guard).expect("catalog service");
        let snapshot = register_and_audit(&service, fixture.root.path(), &source_id);
        service
            .submit(CatalogCommand::Register {
                source_id: source_id.clone(),
                canonical_root: replacement_root.path().to_path_buf(),
                lifecycle_generation: 2,
            })
            .expect("replacement registration admission");
        assert!(matches!(
            receive_until(&service, |event| {
                matches!(
                    event,
                    CatalogEvent::Registered {
                        source_id: registered,
                        lifecycle_generation: 2,
                    } if registered == &source_id
                )
            }),
            CatalogEvent::Registered {
                lifecycle_generation: 2,
                ..
            }
        ));

        let path = RootRelativePath::try_from_path(PathBuf::from("one.wav"))
            .expect("relative curation path");
        service
            .submit(curation_operation(
                &source_id,
                2,
                &snapshot.root_identity,
                1_200,
                SourceCurationOperationFence::ExactRevision(snapshot.committed_revision),
                vec![vec![SourceCurationUpdate::SetRating {
                    path,
                    rating: wavecrate_library::sample_sources::Rating::KEEP_3,
                }]],
            ))
            .expect("replacement operation admission");
        let outcome = receive_curation_operation(&service, 1_200);
        assert!(!outcome.is_success());
        assert_eq!(
            outcome.initial_revision(),
            Some(snapshot.committed_revision)
        );
        assert_eq!(outcome.final_revision(), Some(snapshot.committed_revision));
        assert!(outcome.committed_batches().is_empty());
        assert!(matches!(
            outcome.failure().expect("replacement failure").error(),
            CatalogError::SnapshotUnavailable
        ));
        service.shutdown();
        let database = SourceDatabase::open_for_catalog_with_profile_guard(
            fixture.root.path(),
            &fixture.guard,
            catalog_profile_child(&source_id),
        )
        .expect("reopen replacement database");
        assert_eq!(
            database.get_revision().expect("replacement revision"),
            snapshot.committed_revision
        );
    }

    #[test]
    fn curation_operation_admission_is_bounded_and_payload_credit_is_released() {
        let fixture = Fixture::new("catalog-curation-operation-admission");
        let source_id = SourceId::from_string("source-curation-operation-admission");
        let root_identity = source_root_identity(fixture.root.path()).expect("root identity");
        let (service, test_gate) =
            CatalogService::start_with_test_gate(&fixture.guard).expect("catalog service");
        let path = RootRelativePath::try_from_path(PathBuf::from("one.wav"))
            .expect("relative curation path");
        let update = SourceCurationUpdate::SetRating {
            path,
            rating: wavecrate_library::sample_sources::Rating::KEEP_1,
        };
        assert!(matches!(
            service.submit(curation_operation(
                &source_id,
                1,
                &root_identity,
                1,
                SourceCurationOperationFence::ExactRevision(0),
                Vec::new(),
            )),
            Err(CatalogSubmitError::InvalidCurationRequest(
                CatalogCurationRequestError::Empty
            ))
        ));
        let too_many_batches = (0..=MAX_CURATION_OPERATION_BATCHES)
            .map(|_| vec![update.clone()])
            .collect();
        assert!(matches!(
            service.submit(curation_operation(
                &source_id,
                1,
                &root_identity,
                2,
                SourceCurationOperationFence::ExactRevision(0),
                too_many_batches,
            )),
            Err(CatalogSubmitError::InvalidCurationRequest(
                CatalogCurationRequestError::TooManyOperationBatches { .. }
            ))
        ));

        let pause = test_gate.pause_next();
        service
            .submit(CatalogCommand::Register {
                source_id: source_id.clone(),
                canonical_root: fixture.root.path().to_path_buf(),
                lifecycle_generation: 1,
            })
            .expect("register admission");
        service
            .submit(curation_operation(
                &source_id,
                1,
                &root_identity,
                3,
                SourceCurationOperationFence::ExactRevision(0),
                vec![vec![update]],
            ))
            .expect("bounded operation admission");
        assert!(
            service
                .admission
                .lock()
                .expect("admission lock")
                .curation_payload_bytes
                > 0
        );
        assert_eq!(
            service
                .admission
                .lock()
                .expect("admission lock")
                .curation_update_count,
            1
        );
        pause.release();
        assert!(matches!(
            receive_until(&service, |event| {
                matches!(
                    event,
                    CatalogEvent::CurationOperationCompleted { request_id: 3, .. }
                )
            }),
            CatalogEvent::CurationOperationCompleted { .. }
        ));
        assert_eq!(
            service
                .admission
                .lock()
                .expect("admission lock")
                .curation_payload_bytes,
            0
        );
        assert_eq!(
            service
                .admission
                .lock()
                .expect("admission lock")
                .curation_update_count,
            0
        );
        service.shutdown();
    }

    #[test]
    fn curation_operation_tag_payload_credit_releases_after_terminal_receipt() {
        let fixture = Fixture::new("catalog-curation-operation-tag-credit");
        let source_id = SourceId::from_string("source-curation-operation-tag-credit");
        let root_identity = source_root_identity(fixture.root.path()).expect("root identity");
        let (service, test_gate) =
            CatalogService::start_with_test_gate(&fixture.guard).expect("catalog service");
        let path = RootRelativePath::try_from_path(PathBuf::from("one.wav"))
            .expect("relative curation path");
        let batches = vec![vec![SourceCurationUpdate::AddTag {
            path,
            label: String::from("loop"),
        }]];
        let expected_payload = curation::operation_payload_bytes(&root_identity, &batches);
        let pause = test_gate.pause_next();
        service
            .submit(CatalogCommand::Register {
                source_id: source_id.clone(),
                canonical_root: fixture.root.path().to_path_buf(),
                lifecycle_generation: 1,
            })
            .expect("register admission");
        service
            .submit(curation_operation(
                &source_id,
                1,
                &root_identity,
                4,
                SourceCurationOperationFence::ExactRevision(0),
                batches,
            ))
            .expect("tag operation admission");
        {
            let admission = service.admission.lock().expect("admission lock");
            assert_eq!(admission.curation_payload_bytes, expected_payload);
            assert_eq!(admission.curation_update_count, 1);
        }
        pause.release();
        let outcome = receive_curation_operation(&service, 4);
        assert!(!outcome.is_success());
        {
            let admission = service.admission.lock().expect("admission lock");
            assert_eq!(admission.curation_payload_bytes, 0);
            assert_eq!(admission.curation_update_count, 0);
        }
        service.shutdown();
    }

    #[test]
    fn curation_operation_aggregate_quotas_release_after_multiple_operations() {
        let fixture = Fixture::new("catalog-curation-operation-aggregate-quotas");
        let source_id = SourceId::from_string("source-curation-operation-aggregate-quotas");
        let root_identity = source_root_identity(fixture.root.path()).expect("root identity");
        let (service, test_gate) =
            CatalogService::start_with_test_gate(&fixture.guard).expect("catalog service");
        let short_path =
            RootRelativePath::try_from_path(PathBuf::from("one.wav")).expect("short curation path");
        let half_update_batches =
            repeated_rating_batches(MAX_CURATION_OPERATION_UPDATES / 2, &short_path);
        let pause = test_gate.pause_next();
        service
            .submit(CatalogCommand::Register {
                source_id: source_id.clone(),
                canonical_root: fixture.root.path().to_path_buf(),
                lifecycle_generation: 1,
            })
            .expect("register admission");
        service
            .submit(curation_operation(
                &source_id,
                1,
                &root_identity,
                1_200,
                SourceCurationOperationFence::ExactRevision(0),
                half_update_batches.clone(),
            ))
            .expect("first aggregate update operation admission");
        service
            .submit(curation_operation(
                &source_id,
                1,
                &root_identity,
                1_201,
                SourceCurationOperationFence::ExactRevision(0),
                half_update_batches,
            ))
            .expect("second aggregate update operation admission");
        let one_update = vec![vec![SourceCurationUpdate::SetRating {
            path: short_path.clone(),
            rating: wavecrate_library::sample_sources::Rating::KEEP_1,
        }]];
        assert_eq!(
            service.submit(curation_operation(
                &source_id,
                1,
                &root_identity,
                1_202,
                SourceCurationOperationFence::ExactRevision(0),
                one_update,
            )),
            Err(CatalogSubmitError::Full)
        );
        assert_eq!(
            service
                .admission
                .lock()
                .expect("admission lock")
                .curation_update_count,
            MAX_CURATION_OPERATION_UPDATES
        );
        pause.release();

        assert!(!receive_curation_operation(&service, 1_200).is_success());
        assert!(!receive_curation_operation(&service, 1_201).is_success());
        {
            let admission = service.admission.lock().expect("admission lock");
            assert_eq!(admission.curation_update_count, 0);
            assert_eq!(admission.curation_payload_bytes, 0);
        }

        let long_path = RootRelativePath::try_from_path(PathBuf::from("x".repeat(512)))
            .expect("long curation path");
        let long_batches = repeated_rating_batches(MAX_CURATION_OPERATION_UPDATES / 4, &long_path);
        let pause = test_gate.pause_next();
        service
            .submit(curation_operation(
                &source_id,
                1,
                &root_identity,
                1_300,
                SourceCurationOperationFence::ExactRevision(0),
                long_batches.clone(),
            ))
            .expect("first aggregate byte operation admission");
        {
            let admission = service.admission.lock().expect("admission lock");
            assert!(admission.curation_payload_bytes > MAX_CURATION_OPERATION_RECEIPT_BYTES / 2);
            assert_eq!(
                admission.curation_update_count,
                MAX_CURATION_OPERATION_UPDATES / 4
            );
        }
        assert_eq!(
            service.submit(curation_operation(
                &source_id,
                1,
                &root_identity,
                1_301,
                SourceCurationOperationFence::ExactRevision(0),
                long_batches.clone(),
            )),
            Err(CatalogSubmitError::Full)
        );
        pause.release();
        assert!(!receive_curation_operation(&service, 1_300).is_success());
        {
            let admission = service.admission.lock().expect("admission lock");
            assert_eq!(admission.curation_update_count, 0);
            assert_eq!(admission.curation_payload_bytes, 0);
        }

        service
            .submit(curation_operation(
                &source_id,
                1,
                &root_identity,
                1_301,
                SourceCurationOperationFence::ExactRevision(0),
                long_batches,
            ))
            .expect("aggregate byte operation re-admission");
        assert!(!receive_curation_operation(&service, 1_301).is_success());
        service.shutdown();
        let mut saw_closed = false;
        while let Ok(event) = service.try_recv_event() {
            if matches!(event, CatalogEvent::Closed) {
                saw_closed = true;
            }
        }
        assert!(saw_closed);
        let admission = service.admission.lock().expect("admission lock");
        assert_eq!(admission.event_budget, 0);
        assert_eq!(admission.curation_update_count, 0);
        assert_eq!(admission.curation_payload_bytes, 0);
    }

    #[test]
    fn shutdown_drains_admitted_curation_operation_before_closed() {
        let fixture = Fixture::new("catalog-curation-operation-shutdown");
        write_test_wav(&fixture.root.path().join("one.wav"), 16_000, 4);
        let source_id = SourceId::from_string("source-curation-operation-shutdown");
        let (service, test_gate) =
            CatalogService::start_with_test_gate(&fixture.guard).expect("catalog service");
        let snapshot = register_and_audit(&service, fixture.root.path(), &source_id);
        let pause = test_gate.pause_next();
        service
            .submit(curation_operation(
                &source_id,
                1,
                &snapshot.root_identity,
                950,
                SourceCurationOperationFence::ExactRevision(snapshot.committed_revision),
                vec![vec![SourceCurationUpdate::SetRating {
                    path: RootRelativePath::try_from_path(PathBuf::from("one.wav"))
                        .expect("relative curation path"),
                    rating: wavecrate_library::sample_sources::Rating::KEEP_3,
                }]],
            ))
            .expect("operation admission");
        let service = Arc::new(service);
        let shutdown_service = Arc::clone(&service);
        let shutdown_thread = thread::spawn(move || shutdown_service.shutdown());
        while !admission_state(&service.admission, |state| state.shutdown) {
            thread::yield_now();
        }
        pause.release();
        shutdown_thread.join().expect("catalog shutdown");

        let mut saw_operation = false;
        let mut saw_closed = false;
        while let Ok(event) = service.try_recv_event() {
            match event {
                CatalogEvent::CurationOperationCompleted {
                    request_id: 950,
                    outcome,
                    ..
                } => {
                    assert!(!saw_closed);
                    assert!(outcome.is_success());
                    saw_operation = true;
                }
                CatalogEvent::Closed => {
                    assert!(saw_operation);
                    saw_closed = true;
                }
                event => panic!("unexpected shutdown event: {event:?}"),
            }
        }
        assert!(saw_operation);
        assert!(saw_closed);
        let database = SourceDatabase::open_for_catalog_with_profile_guard(
            fixture.root.path(),
            &fixture.guard,
            catalog_profile_child(&source_id),
        )
        .expect("reopen committed source database");
        assert_eq!(
            database.get_revision().expect("committed source revision"),
            snapshot.committed_revision + 1
        );
    }
}
