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
use wavecrate_library::sample_sources::reconciliation::RootIdentity;
use wavecrate_library::sample_sources::{SourceDatabase, SourceDbError, SourceId};
use wavecrate_scan::sample_sources::scanner::{
    ScanError, ScanWritePhase, ScanWriter,
    audit_manifest_for_catalog_with_budget_and_progress_and_writer,
    read_wav_display_metadata_for_manifest,
};

const COMMAND_CAPACITY: usize = 32;
const EVENT_CAPACITY: usize = 64;
const WORKER_IDLE_POLL: Duration = Duration::from_millis(25);
const DIRECTORY_PAGE_LIMIT: usize = 256;
const CATALOG_CHILD_PREFIX: &str = "wavecrate-catalog-";

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

/// A bounded admission failure from the catalog command queue.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CatalogSubmitError {
    /// The worker command queue is full.
    #[error("catalog command queue is full")]
    Full,
    /// The catalog worker has closed or is shutting down.
    #[error("catalog worker is closed")]
    Closed,
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
}

struct DesiredSource {
    lifecycle_generation: u64,
    active: bool,
    attempt_token: Option<Arc<AtomicBool>>,
}

struct AdmissionState {
    shutdown: bool,
    closed: bool,
    event_budget: usize,
    sources: HashMap<SourceId, DesiredSource>,
}

impl AdmissionState {
    fn new() -> Self {
        Self {
            shutdown: false,
            closed: false,
            event_budget: 1,
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
    events: VecDeque<CatalogEvent>,
    closed: bool,
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

    fn send(&self, event: CatalogEvent) -> bool {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if queue.events.len() >= EVENT_CAPACITY {
            return false;
        }
        queue.events.push_back(event);
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

    fn try_recv(&self) -> Result<CatalogEvent, TryRecvError> {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(event) = queue.events.pop_front() {
            return Ok(event);
        }
        if queue.closed {
            Err(TryRecvError::Disconnected)
        } else {
            Err(TryRecvError::Empty)
        }
    }

    fn recv_timeout(&self, timeout: Duration) -> Result<CatalogEvent, mpsc::RecvTimeoutError> {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(event) = queue.events.pop_front() {
            return Ok(event);
        }
        if queue.closed {
            return Err(mpsc::RecvTimeoutError::Disconnected);
        }
        let (mut queue, wait) = self
            .wake
            .wait_timeout(queue, timeout)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(event) = queue.events.pop_front() {
            return Ok(event);
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
}

impl EventReservation<'_> {
    fn publish(&mut self, event: CatalogEvent) {
        assert!(
            self.hub.send(event),
            "catalog event capacity accounting invariant violated"
        );
        self.remaining = self.remaining.saturating_sub(1);
    }

    fn finish(self) {
        if self.remaining == 0 {
            return;
        }
        let mut state = self
            .admission
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.event_budget = state.event_budget.saturating_sub(self.remaining);
    }
}

fn command_event_budget(command: &CatalogCommand) -> usize {
    match command {
        CatalogCommand::Register { .. } => 2,
        CatalogCommand::RequestAudit { .. }
        | CatalogCommand::ReadSnapshot { .. }
        | CatalogCommand::Remove { .. }
        | CatalogCommand::Shutdown => 1,
    }
}

#[derive(Clone)]
enum AdmissionAction {
    Register {
        source_id: SourceId,
        lifecycle_generation: u64,
    },
    RequestAudit,
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
                    },
                );
            }
        }
        AdmissionAction::Shutdown => {
            state.shutdown = true;
            state.invalidate_all();
        }
        AdmissionAction::RequestAudit | AdmissionAction::ReadSnapshot => {}
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

fn begin_audit_attempt(
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
        let worker_profile_guard = profile_guard.try_clone()?;
        let (commands, command_rx) = mpsc::sync_channel(COMMAND_CAPACITY);
        let events = Arc::new(EventHub::new());
        let worker_events = Arc::clone(&events);
        let admission = Arc::new(Mutex::new(AdmissionState::new()));
        let worker_admission = Arc::clone(&admission);
        let worker = thread::Builder::new()
            .name(String::from("wavecrate-catalog"))
            .spawn(move || {
                run_worker(
                    worker_profile_guard,
                    command_rx,
                    worker_events,
                    worker_admission,
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
        let event_budget = command_event_budget(&command);
        if admission.event_budget.saturating_add(event_budget) > EVENT_CAPACITY {
            return Err(CatalogSubmitError::Full);
        }
        match self.commands.try_send(command) {
            Ok(()) => {
                admission.event_budget += event_budget;
                apply_admission_action(&mut admission, action);
                Ok(())
            }
            Err(TrySendError::Full(_)) => Err(CatalogSubmitError::Full),
            Err(TrySendError::Disconnected(_)) => {
                admission.closed = true;
                Err(CatalogSubmitError::Closed)
            }
        }
    }

    /// Read the next worker event without blocking.
    pub fn try_recv_event(&self) -> Result<CatalogEvent, TryRecvError> {
        let event = self.events.try_recv()?;
        self.release_event_credit();
        Ok(event)
    }

    /// Read the next worker event with a bounded wait.
    pub fn recv_event_timeout(
        &self,
        timeout: Duration,
    ) -> Result<CatalogEvent, mpsc::RecvTimeoutError> {
        let event = self.events.recv_timeout(timeout)?;
        self.release_event_credit();
        Ok(event)
    }

    fn release_event_credit(&self) {
        let mut admission = self
            .admission
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        admission.event_budget = admission.event_budget.saturating_sub(1);
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
                let _ = self.commands.try_send(CatalogCommand::Shutdown);
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
) {
    let mut sources = HashMap::new();
    loop {
        if admission_state(&admission, |state| state.shutdown || state.closed) {
            break;
        }
        let command = match command_rx.recv_timeout(WORKER_IDLE_POLL) {
            Ok(command) => command,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if admission_state(&admission, |state| state.shutdown || state.closed) {
            break;
        }
        let reserved_events = command_event_budget(&command);
        let mut reservation = EventReservation {
            hub: &events,
            admission: &admission,
            remaining: reserved_events,
        };
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
        events.send(CatalogEvent::Closed),
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
    let Some(token) = begin_audit_attempt(admission, &source_id, lifecycle_generation) else {
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
    let (revision, manifest) = database
        .manifest_snapshot_with_revision()
        .map_err(catalog_error_from_db)?;
    if audit_metadata.source_revision != revision {
        return Err(CatalogError::DirectoryTruthUnavailable(format!(
            "audit metadata revision {} does not match manifest revision {revision}",
            audit_metadata.source_revision
        )));
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
    Ok(CatalogSnapshot {
        source_id: source_id.clone(),
        root_identity: root_identity.clone(),
        lifecycle_generation,
        committed_revision: revision,
        directory_generation,
        manifest,
        directories,
        wav_metadata: BTreeMap::new(),
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
}
