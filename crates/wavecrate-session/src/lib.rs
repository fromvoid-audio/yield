#![deny(missing_docs)]
#![deny(warnings)]

//! GPUI-free ownership for the persisted source registry.
//!
//! The session worker owns the persistence profile and the library writer. UI code submits
//! canonical roots and consumes immutable registry snapshots; it never receives a database
//! connection or performs persistence I/O.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use thiserror::Error;
use wavecrate_library::app_dirs::{
    AppRootGuard, PersistenceProfileGuard, ProfileOwnershipError, WritableProfileGuard,
};
use wavecrate_library::sample_sources::{
    GlobalLibraryWriter, GlobalLibraryWriterQueueError, GlobalLibraryWriterStartError,
    GlobalLibraryWriterStatus, LibraryError, SampleSource, SourceRegistrySnapshot,
};

/// The non-live profile used by the GPUI application session.
pub const GPUI_DEVELOPMENT_PROFILE: &str = "gpui-development";

const SESSION_COMMAND_CAPACITY: usize = 32;
const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(10);
const IDLE_STATUS_CHECK_INTERVAL: Duration = Duration::from_millis(250);

/// A root resolved by the scanner and safe to use as a stable source identity.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CanonicalSourceRoot(PathBuf);

impl CanonicalSourceRoot {
    /// Validate a scanner-provided root without touching the filesystem.
    ///
    /// The scanner owns filesystem canonicalization. This boundary only rejects relative paths,
    /// parent components, and current-directory components so persistence never records an
    /// ambiguous identity.
    pub fn new(root: PathBuf) -> Result<Self, RootError> {
        if !root.is_absolute() {
            return Err(RootError::NotAbsolute(root));
        }
        if root
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(RootError::ContainsParent(root));
        }
        if root
            .components()
            .any(|component| matches!(component, Component::CurDir))
        {
            return Err(RootError::ContainsCurrentDirectory(root));
        }
        Ok(Self(root))
    }

    /// Borrow the resolved root path.
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// Consume the identity and return its path.
    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }
}

/// A root rejected at the session persistence boundary.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum RootError {
    /// The path was relative and therefore cannot be a durable source identity.
    #[error("source root is not absolute: {0}")]
    NotAbsolute(PathBuf),
    /// The path contains a parent component and was not scanner-resolved.
    #[error("source root contains a parent component: {0}")]
    ContainsParent(PathBuf),
    /// The path contains a current-directory component and was not scanner-resolved.
    #[error("source root contains a current-directory component: {0}")]
    ContainsCurrentDirectory(PathBuf),
}

/// A monotonically increasing session registry revision.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct RegistryRevision(u64);

impl RegistryRevision {
    /// Return the numeric revision for logging or UI fencing.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// An id assigned to a submitted source mutation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RequestId(u64);

impl RequestId {
    /// Return the numeric request id for logging or UI correlation.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// Errors reported by a session operation or persistence owner.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum SessionError {
    /// Persistence could not be acquired or became unavailable.
    #[error("session persistence unavailable: {0}")]
    Persistence(String),
    /// A queued library operation failed.
    #[error("session library operation failed: {0}")]
    Library(String),
    /// The service was already closed.
    #[error("session service is closed")]
    Closed,
}

/// Errors returned when starting the session worker.
#[derive(Debug, Error)]
pub enum StartError {
    /// The operating system rejected the worker thread.
    #[error("failed to start source session worker: {0}")]
    ThreadSpawn(#[source] std::io::Error),
}

/// Errors returned while submitting a bounded session command.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum SubmitError {
    /// The worker command queue is full.
    #[error("source session command queue is full")]
    Full,
    /// The service has closed command admission.
    #[error("source session service is closed")]
    Closed,
}

/// Errors returned when synchronously draining the session worker.
#[derive(Debug, Error)]
pub enum ShutdownError {
    /// The worker could not receive the shutdown acknowledgement.
    #[error("source session worker stopped before shutdown acknowledgement: {0}")]
    WorkerChannel(String),
    /// The worker reported a persistence or drain failure.
    #[error(transparent)]
    Session(#[from] SessionError),
    /// The worker panicked while shutting down.
    #[error("source session worker panicked during shutdown")]
    WorkerPanicked,
}

/// Events emitted by the session worker.
#[derive(Debug)]
pub enum RegistryEvent {
    /// The profile owner is loading the initial registry snapshot.
    Loading {
        /// Revision associated with the loading state.
        revision: RegistryRevision,
    },
    /// A complete immutable registry projection is ready.
    Snapshot {
        /// Revision represented by the snapshot.
        revision: RegistryRevision,
        /// Ordered source descriptors for the session.
        snapshot: SourceRegistrySnapshot,
    },
    /// A replacement completed for the given revision.
    Persisted {
        /// Revision durably written to the development profile.
        revision: RegistryRevision,
    },
    /// A submitted mutation has been applied to the loaded in-memory projection.
    RequestApplied {
        /// Request that was applied, including an idempotent no-op.
        request: RequestId,
        /// Projection revision after applying the request.
        revision: RegistryRevision,
    },
    /// One requested mutation was rejected.
    RequestFailed {
        /// Request that failed.
        request: RequestId,
        /// Failure reported by the owner.
        error: SessionError,
    },
    /// Persistence failed closed. No live-profile fallback is attempted.
    PersistenceUnavailable {
        /// Last in-memory revision known to the session.
        revision: RegistryRevision,
        /// Failure that disabled durable mutations.
        error: SessionError,
    },
    /// The worker has drained and closed.
    Closed,
}

/// A GPUI-free source-registry session owner.
pub struct SourceRegistryService {
    commands: Mutex<Option<SyncSender<Command>>>,
    closed: AtomicBool,
    next_request: AtomicU64,
    events: Receiver<RegistryEvent>,
    worker: Option<JoinHandle<Result<(), SessionError>>>,
}

impl SourceRegistryService {
    /// Start a session on the fixed non-live `gpui-development` profile.
    pub fn start() -> Result<Self, StartError> {
        Self::start_inner(None)
    }

    /// Submit a source root for durable registration.
    pub fn add_root(&self, root: CanonicalSourceRoot) -> Result<RequestId, SubmitError> {
        self.submit(|request| Command::AddRoot { request, root })
    }

    /// Remove a source by canonical root.
    ///
    /// Root identity is used deliberately so a source can be removed while an add/retained-id
    /// lookup is still pending. An old lookup result is then ordered before this removal and
    /// cannot resurrect the source.
    pub fn remove_root(&self, root: CanonicalSourceRoot) -> Result<RequestId, SubmitError> {
        self.submit(|request| Command::RemoveRoot { request, root })
    }

    /// Poll one session event without blocking the caller.
    pub fn try_recv(&self) -> Result<RegistryEvent, TryRecvError> {
        self.events.try_recv()
    }

    /// Close command admission, drain the final baseline/lookups/save, and synchronously join.
    pub fn shutdown(&mut self) -> Result<(), ShutdownError> {
        self.closed.store(true, Ordering::Release);
        let sender = self
            .commands
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        let Some(sender) = sender else {
            let worker_result = self
                .worker
                .take()
                .map(|worker| worker.join().map_err(|_| ShutdownError::WorkerPanicked))
                .transpose()?
                .unwrap_or(Ok(()));
            return worker_result.map_err(ShutdownError::Session);
        };
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        sender
            .send(Command::Shutdown {
                acknowledgement: ack_tx,
            })
            .map_err(|error| ShutdownError::WorkerChannel(error.to_string()))?;
        drop(sender);
        let result = ack_rx
            .recv()
            .map_err(|error| ShutdownError::WorkerChannel(error.to_string()))?;
        let worker_result = self
            .worker
            .take()
            .map(|worker| worker.join().map_err(|_| ShutdownError::WorkerPanicked))
            .transpose()?
            .unwrap_or(Ok(()));
        result.map_err(ShutdownError::Session)?;
        worker_result.map_err(ShutdownError::Session)
    }

    fn start_inner(app_root_override: Option<PathBuf>) -> Result<Self, StartError> {
        let (command_tx, command_rx) = mpsc::sync_channel(SESSION_COMMAND_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel();
        let worker = thread::Builder::new()
            .name(String::from("wavecrate-source-session"))
            .spawn(move || run_worker(command_rx, event_tx, app_root_override))
            .map_err(StartError::ThreadSpawn)?;
        Ok(Self {
            commands: Mutex::new(Some(command_tx)),
            closed: AtomicBool::new(false),
            next_request: AtomicU64::new(1),
            events: event_rx,
            worker: Some(worker),
        })
    }

    fn submit(&self, command: impl FnOnce(RequestId) -> Command) -> Result<RequestId, SubmitError> {
        let request = RequestId(self.next_request.fetch_add(1, Ordering::Relaxed));
        let commands = self
            .commands
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.closed.load(Ordering::Acquire) {
            return Err(SubmitError::Closed);
        }
        let sender = commands.as_ref().ok_or(SubmitError::Closed)?;
        sender
            .try_send(command(request))
            .map_err(|error| match error {
                TrySendError::Full(_) => SubmitError::Full,
                TrySendError::Disconnected(_) => SubmitError::Closed,
            })?;
        Ok(request)
    }
}

impl Drop for SourceRegistryService {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

enum Command {
    AddRoot {
        request: RequestId,
        root: CanonicalSourceRoot,
    },
    RemoveRoot {
        request: RequestId,
        root: CanonicalSourceRoot,
    },
    Shutdown {
        acknowledgement: SyncSender<Result<(), SessionError>>,
    },
}

struct PendingAdd {
    root: CanonicalSourceRoot,
    receiver: Option<Receiver<Result<Option<SampleSource>, LibraryError>>>,
}

#[derive(Clone)]
enum Edit {
    Add {
        request: RequestId,
        root: CanonicalSourceRoot,
        source: Option<SampleSource>,
    },
    Remove {
        request: RequestId,
        root: CanonicalSourceRoot,
    },
}

struct SaveInFlight {
    revision: RegistryRevision,
    receiver: Receiver<Result<(), LibraryError>>,
}

struct Coordinator {
    loaded: bool,
    baseline: Vec<SampleSource>,
    persisted: Vec<SampleSource>,
    current: Vec<SampleSource>,
    edits: Vec<Edit>,
    revision: RegistryRevision,
}

impl Coordinator {
    fn new() -> Self {
        Self {
            loaded: false,
            baseline: Vec::new(),
            persisted: Vec::new(),
            current: Vec::new(),
            edits: Vec::new(),
            revision: RegistryRevision::default(),
        }
    }

    fn revision(&self) -> RegistryRevision {
        self.revision
    }

    fn add_edit(&mut self, request: RequestId, root: CanonicalSourceRoot) {
        self.edits.push(Edit::Add {
            request,
            root,
            source: None,
        });
    }

    fn remove_edit(&mut self, request: RequestId, root: CanonicalSourceRoot) {
        self.edits.push(Edit::Remove { request, root });
    }

    fn resolve_add(
        &mut self,
        request: RequestId,
        source: SampleSource,
    ) -> Result<bool, SessionError> {
        let Some(edit) = self.edits.iter_mut().find(|edit| {
            matches!(edit, Edit::Add { request: edit_request, .. } if *edit_request == request)
        }) else {
            return Ok(false);
        };
        if let Edit::Add {
            root,
            source: edit_source,
            ..
        } = edit
        {
            let mut source = source;
            source.root = root.clone().into_path_buf();
            *edit_source = Some(source);
        }
        self.rebuild()
    }

    fn fail_add(&mut self, request: RequestId) -> Result<bool, SessionError> {
        let before = self.edits.len();
        self.edits.retain(|edit| {
            !matches!(edit, Edit::Add { request: edit_request, .. } if *edit_request == request)
        });
        if before == self.edits.len() {
            return Ok(false);
        }
        self.rebuild()
    }

    fn load(&mut self, snapshot: SourceRegistrySnapshot) -> Result<bool, SessionError> {
        self.loaded = true;
        self.baseline = snapshot.as_slice().to_vec();
        self.persisted = self.baseline.clone();
        self.current = self.baseline.clone();
        self.revision = RegistryRevision::default();
        self.rebuild()
    }

    fn rebuild(&mut self) -> Result<bool, SessionError> {
        if !self.loaded {
            return Ok(false);
        }
        let previous = self.current.clone();
        let mut next = self.baseline.clone();
        for edit in &self.edits {
            match edit {
                Edit::Add {
                    root,
                    source: Some(source),
                    ..
                } => add_source(&mut next, root, source.clone())?,
                Edit::Add { source: None, .. } => {}
                Edit::Remove { root, .. } => {
                    next.retain(|source| source.root != root.as_path());
                }
            }
        }
        let changed = !sources_equal(&previous, &next);
        self.current = next;
        if changed {
            self.revision = RegistryRevision(self.revision.0.saturating_add(1));
        }
        Ok(changed)
    }

    fn compact_if_resolved(&mut self, pending_adds: bool) {
        if self.loaded && !pending_adds {
            self.baseline = self.current.clone();
            self.edits.clear();
        }
    }

    fn snapshot(&self) -> SourceRegistrySnapshot {
        SourceRegistrySnapshot::new(self.current.clone())
    }

    fn is_dirty(&self) -> bool {
        !sources_equal(&self.current, &self.persisted)
    }

    fn mark_persisted(&mut self) {
        self.persisted = self.current.clone();
    }

    fn accept_save_ack(&mut self, revision: RegistryRevision) -> bool {
        if revision != self.revision {
            return false;
        }
        self.mark_persisted();
        true
    }

    fn resolved_add_requests(&self) -> impl Iterator<Item = RequestId> + '_ {
        self.edits.iter().filter_map(|edit| match edit {
            Edit::Add {
                request,
                source: Some(_),
                ..
            } => Some(*request),
            Edit::Add { source: None, .. } | Edit::Remove { .. } => None,
        })
    }

    fn remove_requests(&self) -> impl Iterator<Item = RequestId> + '_ {
        self.edits.iter().filter_map(|edit| match edit {
            Edit::Remove { request, .. } => Some(*request),
            Edit::Add { .. } => None,
        })
    }

    fn discard_requests(&mut self, requests: &HashSet<RequestId>) -> Result<(), SessionError> {
        if requests.is_empty() {
            return Ok(());
        }
        self.edits.retain(|edit| {
            let request = match edit {
                Edit::Add { request, .. } | Edit::Remove { request, .. } => request,
            };
            !requests.contains(request)
        });
        self.rebuild().map(|_| ())
    }
}

fn add_source(
    sources: &mut Vec<SampleSource>,
    root: &CanonicalSourceRoot,
    source: SampleSource,
) -> Result<(), SessionError> {
    if sources
        .iter()
        .any(|existing| existing.root == root.as_path())
    {
        return Ok(());
    }
    if sources
        .iter()
        .any(|existing| existing.id == source.id && existing.root != source.root)
    {
        return Err(SessionError::Library(format!(
            "source id {} is already assigned to another root",
            source.id
        )));
    }
    sources.push(source);
    Ok(())
}

fn sources_equal(left: &[SampleSource], right: &[SampleSource]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left.id == right.id
                && left.root == right.root
                && left.role == right.role
                && left.metadata_storage == right.metadata_storage
                && left.primary_import_folder == right.primary_import_folder
        })
}

struct Persistence {
    writable: Option<WritableProfileGuard>,
    writer: Option<GlobalLibraryWriter>,
    load: Option<Receiver<Result<SourceRegistrySnapshot, LibraryError>>>,
    pending_adds: HashMap<RequestId, PendingAdd>,
    save: Option<SaveInFlight>,
    force_save: bool,
    unavailable: Option<SessionError>,
}

impl Persistence {
    fn start(
        app_root_override: Option<PathBuf>,
    ) -> (Self, Option<AppRootGuard>, PersistenceProfileGuard) {
        let profile_guard = PersistenceProfileGuard::named(GPUI_DEVELOPMENT_PROFILE);
        let mut persistence = Self {
            writable: None,
            writer: None,
            load: None,
            pending_adds: HashMap::new(),
            save: None,
            force_save: false,
            unavailable: None,
        };
        let root_guard = match app_root_override {
            Some(root) => match AppRootGuard::set(root) {
                Ok(guard) => Some(guard),
                Err(error) => {
                    persistence.unavailable = Some(SessionError::Persistence(error.to_string()));
                    None
                }
            },
            None => None,
        };
        if persistence.unavailable.is_none() {
            match WritableProfileGuard::acquire_current() {
                Ok(writable) => match GlobalLibraryWriter::start(&writable) {
                    Ok(writer) => {
                        persistence.writable = Some(writable);
                        persistence.writer = Some(writer);
                    }
                    Err(error) => persistence.unavailable = Some(session_from_start_error(error)),
                },
                Err(error) => persistence.unavailable = Some(session_from_profile_error(error)),
            }
        }
        (persistence, root_guard, profile_guard)
    }

    fn status_error(&self) -> Option<SessionError> {
        self.unavailable.clone().or_else(|| {
            self.writer
                .as_ref()
                .and_then(|writer| match writer.status() {
                    GlobalLibraryWriterStatus::Unavailable { reason } => {
                        Some(SessionError::Persistence(reason.to_string()))
                    }
                    GlobalLibraryWriterStatus::Closed => Some(SessionError::Persistence(
                        String::from("global library writer closed"),
                    )),
                    _ => None,
                })
        })
    }

    fn available(&self) -> bool {
        self.writer
            .as_ref()
            .is_some_and(|writer| matches!(writer.status(), GlobalLibraryWriterStatus::Available))
    }

    fn shutdown(&mut self) {
        if let Some(mut writer) = self.writer.take() {
            let _ = writer.shutdown();
        }
        self.save = None;
        self.load = None;
        self.pending_adds.clear();
        self.writable.take();
    }
}

fn session_from_profile_error(error: ProfileOwnershipError) -> SessionError {
    SessionError::Persistence(error.to_string())
}

fn session_from_start_error(error: GlobalLibraryWriterStartError) -> SessionError {
    SessionError::Persistence(error.to_string())
}

fn session_from_queue_error(error: GlobalLibraryWriterQueueError) -> SessionError {
    SessionError::Persistence(error.to_string())
}

fn run_worker(
    command_rx: Receiver<Command>,
    event_tx: mpsc::Sender<RegistryEvent>,
    app_root_override: Option<PathBuf>,
) -> Result<(), SessionError> {
    let (mut persistence, _root_guard, _profile_guard) = Persistence::start(app_root_override);
    let mut coordinator = Coordinator::new();
    let mut applied_requests = HashSet::new();
    let mut unfinalized_requests = HashSet::new();
    let _ = event_tx.send(RegistryEvent::Loading {
        revision: coordinator.revision(),
    });
    let mut reported_unavailable = false;
    if let Some(error) = persistence.status_error() {
        let revision = coordinator.revision();
        report_unavailable(
            &mut persistence,
            &mut coordinator,
            &event_tx,
            revision,
            error,
            &mut reported_unavailable,
            &mut unfinalized_requests,
        );
    }

    loop {
        drive(
            &mut persistence,
            &mut coordinator,
            &event_tx,
            &mut reported_unavailable,
            &mut applied_requests,
            &mut unfinalized_requests,
        );
        let command = if can_block_on_command(&persistence, &coordinator) {
            match command_rx.recv_timeout(IDLE_STATUS_CHECK_INTERVAL) {
                Ok(command) => Some(command),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => None,
            }
        } else {
            match command_rx.recv_timeout(WORKER_POLL_INTERVAL) {
                Ok(command) => Some(command),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => None,
            }
        };
        let Some(command) = command else {
            let result = drain_shutdown(
                &mut persistence,
                &mut coordinator,
                &event_tx,
                &mut reported_unavailable,
                &mut applied_requests,
                &mut unfinalized_requests,
            );
            persistence.shutdown();
            let _ = event_tx.send(RegistryEvent::Closed);
            return result;
        };
        if let Some(request) = command_request(&command) {
            unfinalized_requests.insert(request);
            if let Some(error) = persistence.status_error() {
                let revision = coordinator.revision();
                report_unavailable(
                    &mut persistence,
                    &mut coordinator,
                    &event_tx,
                    revision,
                    error,
                    &mut reported_unavailable,
                    &mut unfinalized_requests,
                );
                continue;
            }
        }
        match command {
            Command::AddRoot { request, root } => {
                coordinator.add_edit(request, root.clone());
                persistence.pending_adds.insert(
                    request,
                    PendingAdd {
                        root,
                        receiver: None,
                    },
                );
            }
            Command::RemoveRoot { request, root } => {
                coordinator.remove_edit(request, root);
                match coordinator.rebuild() {
                    Ok(changed) => {
                        if changed {
                            let _ = event_tx.send(RegistryEvent::Snapshot {
                                revision: coordinator.revision(),
                                snapshot: coordinator.snapshot(),
                            });
                        }
                        if coordinator.loaded {
                            announce_request(
                                &event_tx,
                                &mut applied_requests,
                                &mut unfinalized_requests,
                                request,
                                coordinator.revision(),
                            );
                        }
                    }
                    Err(error) => {
                        let _ = coordinator.discard_requests(&HashSet::from([request]));
                        fail_request(&event_tx, &mut unfinalized_requests, request, error);
                    }
                }
                coordinator.compact_if_resolved(!persistence.pending_adds.is_empty());
            }
            Command::Shutdown { acknowledgement } => {
                let result = drain_shutdown(
                    &mut persistence,
                    &mut coordinator,
                    &event_tx,
                    &mut reported_unavailable,
                    &mut applied_requests,
                    &mut unfinalized_requests,
                );
                persistence.shutdown();
                let _ = event_tx.send(RegistryEvent::Closed);
                let _ = acknowledgement.send(result.clone().map_err(|error| error.clone()));
                return result;
            }
        }
    }
}

fn can_block_on_command(persistence: &Persistence, coordinator: &Coordinator) -> bool {
    persistence.unavailable.is_some()
        || (coordinator.loaded
            && persistence.load.is_none()
            && persistence.pending_adds.is_empty()
            && persistence.save.is_none()
            && !persistence.force_save
            && !coordinator.is_dirty()
            && persistence.unavailable.is_none())
}

fn command_request(command: &Command) -> Option<RequestId> {
    match command {
        Command::AddRoot { request, .. } | Command::RemoveRoot { request, .. } => Some(*request),
        Command::Shutdown { .. } => None,
    }
}

fn drive(
    persistence: &mut Persistence,
    coordinator: &mut Coordinator,
    event_tx: &mpsc::Sender<RegistryEvent>,
    reported_unavailable: &mut bool,
    applied_requests: &mut HashSet<RequestId>,
    unfinalized_requests: &mut HashSet<RequestId>,
) {
    if persistence.unavailable.is_some() {
        let error = persistence
            .unavailable
            .clone()
            .expect("unavailable state retained");
        report_unavailable(
            persistence,
            coordinator,
            event_tx,
            coordinator.revision(),
            error,
            reported_unavailable,
            unfinalized_requests,
        );
        return;
    }
    if let Some(error) = persistence.status_error() {
        report_unavailable(
            persistence,
            coordinator,
            event_tx,
            coordinator.revision(),
            error,
            reported_unavailable,
            unfinalized_requests,
        );
        return;
    }
    queue_load(persistence, coordinator);
    poll_load(
        persistence,
        coordinator,
        event_tx,
        reported_unavailable,
        applied_requests,
        unfinalized_requests,
    );
    queue_lookups(persistence);
    poll_lookups(
        persistence,
        coordinator,
        event_tx,
        applied_requests,
        unfinalized_requests,
    );
    queue_save(persistence, coordinator);
    poll_save(
        persistence,
        coordinator,
        event_tx,
        reported_unavailable,
        unfinalized_requests,
    );
    queue_save(persistence, coordinator);
    if let Some(error) = persistence.unavailable.clone() {
        report_unavailable(
            persistence,
            coordinator,
            event_tx,
            coordinator.revision(),
            error,
            reported_unavailable,
            unfinalized_requests,
        );
    }
}

fn queue_load(persistence: &mut Persistence, coordinator: &Coordinator) {
    if coordinator.loaded || persistence.load.is_some() || !persistence.available() {
        return;
    }
    let Some(writer) = persistence.writer.as_ref() else {
        return;
    };
    match writer.load_source_registry_snapshot() {
        Ok(receiver) => persistence.load = Some(receiver),
        Err(GlobalLibraryWriterQueueError::Initializing | GlobalLibraryWriterQueueError::Full) => {}
        Err(error) => persistence.unavailable = Some(session_from_queue_error(error)),
    }
}

fn poll_load(
    persistence: &mut Persistence,
    coordinator: &mut Coordinator,
    event_tx: &mpsc::Sender<RegistryEvent>,
    reported_unavailable: &mut bool,
    applied_requests: &mut HashSet<RequestId>,
    unfinalized_requests: &mut HashSet<RequestId>,
) {
    let Some(receiver) = persistence.load.as_ref() else {
        return;
    };
    match receiver.try_recv() {
        Ok(Ok(snapshot)) => {
            persistence.load = None;
            match coordinator.load(snapshot) {
                Ok(_) => {
                    let _ = event_tx.send(RegistryEvent::Snapshot {
                        revision: coordinator.revision(),
                        snapshot: coordinator.snapshot(),
                    });
                    let _ = event_tx.send(RegistryEvent::Persisted {
                        revision: RegistryRevision::default(),
                    });
                    announce_resolved_requests(
                        coordinator,
                        event_tx,
                        applied_requests,
                        unfinalized_requests,
                    );
                    coordinator.compact_if_resolved(!persistence.pending_adds.is_empty());
                }
                Err(error) => report_unavailable(
                    persistence,
                    coordinator,
                    event_tx,
                    coordinator.revision(),
                    error,
                    reported_unavailable,
                    unfinalized_requests,
                ),
            }
        }
        Ok(Err(error)) => {
            persistence.load = None;
            report_unavailable(
                persistence,
                coordinator,
                event_tx,
                coordinator.revision(),
                SessionError::Library(error.to_string()),
                reported_unavailable,
                unfinalized_requests,
            );
        }
        Err(TryRecvError::Empty) => {}
        Err(TryRecvError::Disconnected) => {
            persistence.load = None;
            report_unavailable(
                persistence,
                coordinator,
                event_tx,
                coordinator.revision(),
                SessionError::Persistence(String::from(
                    "source registry load receiver disconnected",
                )),
                reported_unavailable,
                unfinalized_requests,
            );
        }
    }
}

fn queue_lookups(persistence: &mut Persistence) {
    if !persistence.available() {
        return;
    }
    let Some(writer) = persistence.writer.as_ref() else {
        return;
    };
    let mut fatal_error = None;
    for pending in persistence.pending_adds.values_mut() {
        if pending.receiver.is_some() {
            continue;
        }
        match writer.lookup_retained_source_for_root(pending.root.clone().into_path_buf()) {
            Ok(receiver) => pending.receiver = Some(receiver),
            Err(
                GlobalLibraryWriterQueueError::Initializing | GlobalLibraryWriterQueueError::Full,
            ) => {
                break;
            }
            Err(error) => {
                fatal_error = Some(session_from_queue_error(error));
                break;
            }
        }
    }
    if let Some(error) = fatal_error {
        persistence.unavailable = Some(error);
    }
}

fn poll_lookups(
    persistence: &mut Persistence,
    coordinator: &mut Coordinator,
    event_tx: &mpsc::Sender<RegistryEvent>,
    applied_requests: &mut HashSet<RequestId>,
    unfinalized_requests: &mut HashSet<RequestId>,
) {
    let mut completed = Vec::new();
    for (request, pending) in &mut persistence.pending_adds {
        let Some(receiver) = pending.receiver.as_ref() else {
            continue;
        };
        match receiver.try_recv() {
            Ok(result) => completed.push((*request, Ok(result))),
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => completed.push((
                *request,
                Err(SessionError::Persistence(String::from(
                    "retained source lookup receiver disconnected",
                ))),
            )),
        }
    }
    for (request, result) in completed {
        persistence.pending_adds.remove(&request);
        match result {
            Ok(Ok(Some(source))) => match coordinator.resolve_add(request, source) {
                Ok(changed) => {
                    if changed {
                        let _ = event_tx.send(RegistryEvent::Snapshot {
                            revision: coordinator.revision(),
                            snapshot: coordinator.snapshot(),
                        });
                    }
                    if coordinator.loaded {
                        announce_request(
                            event_tx,
                            applied_requests,
                            unfinalized_requests,
                            request,
                            coordinator.revision(),
                        );
                    }
                }
                Err(error) => {
                    let _ = coordinator.fail_add(request);
                    fail_request(event_tx, unfinalized_requests, request, error);
                }
            },
            Ok(Ok(None)) => {
                let Some(edit) = coordinator
                    .edits
                    .iter()
                    .find(|edit| {
                        matches!(edit, Edit::Add { request: edit_request, .. } if *edit_request == request)
                    })
                    .cloned()
                else {
                    continue;
                };
                let Edit::Add { root, .. } = edit else {
                    continue;
                };
                let source = SampleSource::new(root.into_path_buf());
                match coordinator.resolve_add(request, source) {
                    Ok(changed) => {
                        if changed {
                            let _ = event_tx.send(RegistryEvent::Snapshot {
                                revision: coordinator.revision(),
                                snapshot: coordinator.snapshot(),
                            });
                        }
                        if coordinator.loaded {
                            announce_request(
                                event_tx,
                                applied_requests,
                                unfinalized_requests,
                                request,
                                coordinator.revision(),
                            );
                        }
                    }
                    Err(error) => {
                        let _ = coordinator.fail_add(request);
                        fail_request(event_tx, unfinalized_requests, request, error);
                    }
                }
            }
            Ok(Err(error)) => {
                let _ = coordinator.fail_add(request);
                fail_request(
                    event_tx,
                    unfinalized_requests,
                    request,
                    SessionError::Library(error.to_string()),
                );
            }
            Err(error) => {
                let _ = coordinator.fail_add(request);
                fail_request(event_tx, unfinalized_requests, request, error);
            }
        }
        let pending_adds = !persistence.pending_adds.is_empty();
        coordinator.compact_if_resolved(pending_adds);
    }
}

fn queue_save(persistence: &mut Persistence, coordinator: &Coordinator) {
    if !coordinator.loaded
        || !persistence.pending_adds.is_empty()
        || persistence.save.is_some()
        || !persistence.available()
    {
        return;
    }
    if !coordinator.is_dirty() && !persistence.force_save {
        return;
    }
    let Some(writer) = persistence.writer.as_ref() else {
        return;
    };
    match writer.replace_source_registry(coordinator.snapshot()) {
        Ok(receiver) => {
            persistence.save = Some(SaveInFlight {
                revision: coordinator.revision(),
                receiver,
            });
            persistence.force_save = false;
        }
        Err(GlobalLibraryWriterQueueError::Initializing | GlobalLibraryWriterQueueError::Full) => {}
        Err(error) => persistence.unavailable = Some(session_from_queue_error(error)),
    }
}

fn poll_save(
    persistence: &mut Persistence,
    coordinator: &mut Coordinator,
    event_tx: &mpsc::Sender<RegistryEvent>,
    reported_unavailable: &mut bool,
    unfinalized_requests: &mut HashSet<RequestId>,
) {
    let Some(save) = persistence.save.as_ref() else {
        return;
    };
    match save.receiver.try_recv() {
        Ok(Ok(())) => {
            let revision = save.revision;
            persistence.save = None;
            if coordinator.accept_save_ack(revision) {
                let _ = event_tx.send(RegistryEvent::Persisted { revision });
            } else {
                persistence.force_save = true;
            }
        }
        Ok(Err(error)) => {
            persistence.save = None;
            report_unavailable(
                persistence,
                coordinator,
                event_tx,
                coordinator.revision(),
                SessionError::Library(error.to_string()),
                reported_unavailable,
                unfinalized_requests,
            );
        }
        Err(TryRecvError::Empty) => {}
        Err(TryRecvError::Disconnected) => {
            persistence.save = None;
            report_unavailable(
                persistence,
                coordinator,
                event_tx,
                coordinator.revision(),
                SessionError::Persistence(String::from(
                    "source registry save receiver disconnected",
                )),
                reported_unavailable,
                unfinalized_requests,
            );
        }
    }
}

fn report_unavailable(
    persistence: &mut Persistence,
    coordinator: &mut Coordinator,
    event_tx: &mpsc::Sender<RegistryEvent>,
    revision: RegistryRevision,
    error: SessionError,
    reported: &mut bool,
    unfinalized_requests: &mut HashSet<RequestId>,
) {
    persistence.unavailable = Some(error.clone());
    let mut failed_requests: Vec<_> = unfinalized_requests.drain().collect();
    failed_requests.sort_by_key(|request| request.get());
    for request in failed_requests.iter().copied() {
        let _ = event_tx.send(RegistryEvent::RequestFailed {
            request,
            error: error.clone(),
        });
    }
    let failed_requests: HashSet<_> = failed_requests.into_iter().collect();
    persistence.pending_adds.clear();
    let _ = coordinator.discard_requests(&failed_requests);
    coordinator.compact_if_resolved(false);
    if !*reported {
        let _ = event_tx.send(RegistryEvent::PersistenceUnavailable { revision, error });
        *reported = true;
    }
}

fn fail_request(
    event_tx: &mpsc::Sender<RegistryEvent>,
    unfinalized_requests: &mut HashSet<RequestId>,
    request: RequestId,
    error: SessionError,
) {
    if unfinalized_requests.remove(&request) {
        let _ = event_tx.send(RegistryEvent::RequestFailed { request, error });
    }
}

fn announce_request(
    event_tx: &mpsc::Sender<RegistryEvent>,
    applied_requests: &mut HashSet<RequestId>,
    unfinalized_requests: &mut HashSet<RequestId>,
    request: RequestId,
    revision: RegistryRevision,
) {
    unfinalized_requests.remove(&request);
    if applied_requests.insert(request) {
        let _ = event_tx.send(RegistryEvent::RequestApplied { request, revision });
    }
}

fn announce_resolved_requests(
    coordinator: &Coordinator,
    event_tx: &mpsc::Sender<RegistryEvent>,
    applied_requests: &mut HashSet<RequestId>,
    unfinalized_requests: &mut HashSet<RequestId>,
) {
    let revision = coordinator.revision();
    for request in coordinator
        .resolved_add_requests()
        .chain(coordinator.remove_requests())
    {
        announce_request(
            event_tx,
            applied_requests,
            unfinalized_requests,
            request,
            revision,
        );
    }
}

fn drain_shutdown(
    persistence: &mut Persistence,
    coordinator: &mut Coordinator,
    event_tx: &mpsc::Sender<RegistryEvent>,
    reported_unavailable: &mut bool,
    applied_requests: &mut HashSet<RequestId>,
    unfinalized_requests: &mut HashSet<RequestId>,
) -> Result<(), SessionError> {
    loop {
        if let Some(error) = persistence.status_error() {
            report_unavailable(
                persistence,
                coordinator,
                event_tx,
                coordinator.revision(),
                error.clone(),
                reported_unavailable,
                unfinalized_requests,
            );
            return Err(error);
        }
        drive(
            persistence,
            coordinator,
            event_tx,
            reported_unavailable,
            applied_requests,
            unfinalized_requests,
        );
        if persistence.unavailable.is_some() {
            return Err(persistence
                .unavailable
                .clone()
                .expect("unavailable state retained"));
        }
        if coordinator.loaded
            && persistence.load.is_none()
            && persistence.pending_adds.is_empty()
            && persistence.save.is_none()
            && !persistence.force_save
            && !coordinator.is_dirty()
        {
            return Ok(());
        }
        thread::sleep(WORKER_POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Instant;
    use tempfile::TempDir;
    use wavecrate_library::app_dirs::{ConfigBaseGuard, PROFILE_DIR_NAME};
    use wavecrate_library::sample_sources::{SourceId, SourceMetadataStorage};

    fn root(temp: &TempDir, name: &str) -> CanonicalSourceRoot {
        CanonicalSourceRoot::new(
            temp.path()
                .join(name)
                .canonicalize()
                .unwrap_or_else(|_| temp.path().join(name)),
        )
        .expect("absolute fixture root")
    }

    fn source(root: &CanonicalSourceRoot, id: &str) -> SampleSource {
        SampleSource::new_with_id(SourceId::from_string(id), root.as_path().to_path_buf())
    }

    fn profile_root(temp: &TempDir) -> PathBuf {
        let root = temp
            .path()
            .join(".wavecrate")
            .join(PROFILE_DIR_NAME)
            .join(GPUI_DEVELOPMENT_PROFILE);
        fs::create_dir_all(&root).expect("profile root");
        fs::canonicalize(root).expect("canonical profile root")
    }

    fn wait_for_event(service: &SourceRegistryService, predicate: impl Fn(&RegistryEvent) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match service.try_recv() {
                Ok(event) if predicate(&event) => return,
                Ok(_) => {}
                Err(TryRecvError::Empty) => {
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting for session event"
                    );
                    thread::sleep(Duration::from_millis(5));
                }
                Err(TryRecvError::Disconnected) => panic!("session event channel disconnected"),
            }
        }
    }

    #[test]
    fn canonical_root_rejects_ambiguous_paths() {
        assert!(matches!(
            CanonicalSourceRoot::new(PathBuf::from("relative")),
            Err(RootError::NotAbsolute(_))
        ));
        assert!(matches!(
            CanonicalSourceRoot::new(PathBuf::from("/tmp/../source")),
            Err(RootError::ContainsParent(_))
        ));
    }

    #[test]
    fn staged_remove_root_prevents_pending_add_resurrection() {
        let temp = TempDir::new().expect("temp");
        let add_root = root(&temp, "add");
        let mut coordinator = Coordinator::new();
        let add_request = RequestId(1);
        coordinator.add_edit(add_request, add_root.clone());
        coordinator.remove_edit(RequestId(2), add_root.clone());
        coordinator
            .load(SourceRegistrySnapshot::new(Vec::new()))
            .expect("load");
        coordinator
            .resolve_add(add_request, source(&add_root, "stable-id"))
            .expect("resolve");
        assert!(coordinator.current.is_empty());
    }

    #[test]
    fn coalesced_revision_does_not_accept_stale_save_ack() {
        let temp = TempDir::new().expect("temp");
        let first = root(&temp, "first");
        let second = root(&temp, "second");
        let mut coordinator = Coordinator::new();
        coordinator
            .load(SourceRegistrySnapshot::new(Vec::new()))
            .expect("load");
        coordinator.add_edit(RequestId(1), first.clone());
        coordinator
            .resolve_add(RequestId(1), source(&first, "one"))
            .expect("first add");
        let first_revision = coordinator.revision();
        coordinator.mark_persisted();
        coordinator.add_edit(RequestId(2), second.clone());
        coordinator
            .resolve_add(RequestId(2), source(&second, "two"))
            .expect("second add");
        assert!(coordinator.revision() > first_revision);
        assert!(!coordinator.accept_save_ack(first_revision));
        assert!(coordinator.is_dirty());
        assert_eq!(coordinator.current.len(), 2);
    }

    #[test]
    fn service_uses_isolated_profile_and_reopens_registry() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile_root = profile_root(&temp);
        let mut service =
            SourceRegistryService::start_inner(Some(profile_root.clone())).expect("service start");
        wait_for_event(&service, |event| {
            matches!(event, RegistryEvent::Snapshot { .. })
        });
        let source_root = root(&temp, "persistent");
        let add_request = service.add_root(source_root.clone()).expect("add");
        wait_for_event(&service, |event| {
            matches!(
                event,
                RegistryEvent::RequestApplied { request, .. } if *request == add_request
            )
        });
        wait_for_event(
            &service,
            |event| matches!(event, RegistryEvent::Persisted { revision } if revision.get() > 0),
        );
        service.shutdown().expect("shutdown");

        let development_db = temp
            .path()
            .join(".wavecrate")
            .join(PROFILE_DIR_NAME)
            .join(GPUI_DEVELOPMENT_PROFILE)
            .join("library.db");
        assert!(development_db.is_file());
        assert!(!temp.path().join(".wavecrate/library.db").exists());

        let mut reopened =
            SourceRegistryService::start_inner(Some(profile_root)).expect("reopen service");
        let restored = wait_for_snapshot(&reopened, |snapshot| {
            snapshot
                .as_slice()
                .iter()
                .any(|source| source.root == source_root.as_path())
        });
        assert_eq!(restored.len(), 1);
        reopened.shutdown().expect("reopen shutdown");
    }

    #[test]
    fn existing_remove_is_durable_across_reopen() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile_root = profile_root(&temp);
        let mut service =
            SourceRegistryService::start_inner(Some(profile_root.clone())).expect("service start");
        wait_for_event(&service, |event| {
            matches!(event, RegistryEvent::Snapshot { .. })
        });
        let source_root = root(&temp, "durable-remove");
        let add_request = service.add_root(source_root.clone()).expect("add");
        wait_for_event(&service, |event| {
            matches!(
                event,
                RegistryEvent::RequestApplied { request, .. } if *request == add_request
            )
        });
        wait_for_event(
            &service,
            |event| matches!(event, RegistryEvent::Persisted { revision } if revision.get() > 0),
        );

        let remove_request = service.remove_root(source_root.clone()).expect("remove");
        wait_for_event(&service, |event| {
            matches!(
                event,
                RegistryEvent::RequestApplied { request, .. } if *request == remove_request
            )
        });
        wait_for_event(
            &service,
            |event| matches!(event, RegistryEvent::Persisted { revision } if revision.get() > 0),
        );
        service.shutdown().expect("shutdown");

        let mut reopened =
            SourceRegistryService::start_inner(Some(profile_root)).expect("reopen service");
        let restored = wait_for_snapshot(&reopened, |snapshot| snapshot.as_slice().is_empty());
        assert!(restored.as_slice().is_empty());
        reopened.shutdown().expect("reopen shutdown");
    }

    #[test]
    fn shutdown_drains_without_an_event_consumer() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile_root = profile_root(&temp);
        let mut service =
            SourceRegistryService::start_inner(Some(profile_root.clone())).expect("service start");
        let source_root = root(&temp, "undrained");
        service.add_root(source_root).expect("add");
        service.shutdown().expect("synchronous shutdown");
        assert!(profile_root.join("library.db").is_file());
    }

    #[test]
    fn profile_ownership_conflict_is_reported_without_live_fallback() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile_root = profile_root(&temp);
        let mut owner =
            SourceRegistryService::start_inner(Some(profile_root.clone())).expect("owner service");
        wait_for_event(&owner, |event| {
            matches!(event, RegistryEvent::Snapshot { .. })
        });
        let mut contender =
            SourceRegistryService::start_inner(Some(profile_root)).expect("contender service");
        wait_for_event(&contender, |event| {
            matches!(event, RegistryEvent::PersistenceUnavailable { .. })
        });
        assert!(matches!(
            contender.shutdown(),
            Err(ShutdownError::Session(SessionError::Persistence(_)))
        ));
        owner.shutdown().expect("owner shutdown");
    }

    #[test]
    fn invalid_app_root_fails_closed_without_live_fallback() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let invalid_root = temp.path().join("app-root-file");
        fs::write(&invalid_root, b"not a directory").expect("invalid app root fixture");
        let mut service =
            SourceRegistryService::start_inner(Some(invalid_root.clone())).expect("service start");
        wait_for_event(&service, |event| {
            matches!(event, RegistryEvent::PersistenceUnavailable { .. })
        });
        assert!(matches!(
            service.shutdown(),
            Err(ShutdownError::Session(SessionError::Persistence(_)))
        ));
        assert!(invalid_root.is_file());
        assert!(!temp.path().join(".wavecrate").exists());
    }

    #[cfg(unix)]
    #[test]
    fn idle_profile_replacement_fails_admitted_requests_once() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile_root = profile_root(&temp);
        let mut service =
            SourceRegistryService::start_inner(Some(profile_root.clone())).expect("service start");
        wait_for_event(&service, |event| {
            matches!(event, RegistryEvent::Snapshot { .. })
        });
        wait_for_event(
            &service,
            |event| matches!(event, RegistryEvent::Persisted { revision } if revision.get() == 0),
        );

        let displaced = temp.path().join("displaced-profile");
        fs::rename(&profile_root, &displaced).expect("displace profile");
        fs::create_dir(&profile_root).expect("replacement profile");
        wait_for_event(&service, |event| {
            matches!(event, RegistryEvent::PersistenceUnavailable { .. })
        });

        let add_request = service
            .add_root(root(&temp, "after-replacement-add"))
            .expect("accepted add");
        let remove_request = service
            .remove_root(root(&temp, "after-replacement-remove"))
            .expect("accepted remove");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut add_failures = 0;
        let mut remove_failures = 0;
        while Instant::now() < deadline && (add_failures == 0 || remove_failures == 0) {
            match service.try_recv() {
                Ok(RegistryEvent::RequestFailed { request, error }) => {
                    assert!(matches!(error, SessionError::Persistence(_)));
                    if request == add_request {
                        add_failures += 1;
                    } else if request == remove_request {
                        remove_failures += 1;
                    }
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) => thread::sleep(Duration::from_millis(5)),
                Err(TryRecvError::Disconnected) => {
                    panic!("session event channel disconnected")
                }
            }
        }
        assert_eq!(add_failures, 1, "add request did not fail exactly once");
        assert_eq!(
            remove_failures, 1,
            "remove request did not fail exactly once"
        );

        let quiet_deadline = Instant::now() + Duration::from_millis(100);
        while Instant::now() < quiet_deadline {
            match service.try_recv() {
                Ok(RegistryEvent::RequestFailed { request, .. }) if request == add_request => {
                    add_failures += 1;
                }
                Ok(RegistryEvent::RequestFailed { request, .. }) if request == remove_request => {
                    remove_failures += 1;
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) => thread::sleep(Duration::from_millis(5)),
                Err(TryRecvError::Disconnected) => break,
            }
        }
        assert_eq!(add_failures, 1, "add request emitted a duplicate failure");
        assert_eq!(
            remove_failures, 1,
            "remove request emitted a duplicate failure"
        );
        assert!(matches!(
            service.shutdown(),
            Err(ShutdownError::Session(SessionError::Persistence(_)))
        ));
    }

    #[test]
    fn unavailable_finalizes_staged_requests_once_before_global_error() {
        let temp = TempDir::new().expect("fixtures");
        let first_root = root(&temp, "first");
        let second_root = root(&temp, "second");
        let first_request = RequestId(1);
        let second_request = RequestId(2);
        let mut coordinator = Coordinator::new();
        coordinator
            .load(SourceRegistrySnapshot::new(Vec::new()))
            .expect("load coordinator");
        coordinator.add_edit(first_request, first_root);
        coordinator.remove_edit(second_request, second_root);
        let mut persistence = Persistence {
            writable: None,
            writer: None,
            load: None,
            pending_adds: HashMap::new(),
            save: None,
            force_save: false,
            unavailable: None,
        };
        let (event_tx, event_rx) = mpsc::channel();
        let mut reported = false;
        let mut unfinalized = HashSet::from([first_request, second_request]);
        let error = SessionError::Persistence(String::from("profile replaced"));
        let revision = coordinator.revision();
        report_unavailable(
            &mut persistence,
            &mut coordinator,
            &event_tx,
            revision,
            error.clone(),
            &mut reported,
            &mut unfinalized,
        );
        let revision = coordinator.revision();
        report_unavailable(
            &mut persistence,
            &mut coordinator,
            &event_tx,
            revision,
            error,
            &mut reported,
            &mut unfinalized,
        );

        let events: Vec<_> = event_rx.try_iter().collect();
        assert_eq!(events.len(), 3);
        assert!(matches!(
            &events[0],
            RegistryEvent::RequestFailed { request, .. } if *request == first_request
        ));
        assert!(matches!(
            &events[1],
            RegistryEvent::RequestFailed { request, .. } if *request == second_request
        ));
        assert!(matches!(
            &events[2],
            RegistryEvent::PersistenceUnavailable { .. }
        ));
        assert!(unfinalized.is_empty());
        assert!(persistence.pending_adds.is_empty());
        assert!(coordinator.edits.is_empty());
    }

    #[test]
    fn retained_lookup_preserves_descriptor_policy_on_readd() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let mut service =
            SourceRegistryService::start_inner(Some(profile_root(&temp))).expect("service start");
        wait_for_event(&service, |event| {
            matches!(event, RegistryEvent::Snapshot { .. })
        });
        let source_root = root(&temp, "protected");
        let request = service.add_root(source_root.clone()).expect("add");
        let snapshot = wait_for_snapshot(&service, |snapshot| {
            snapshot
                .as_slice()
                .iter()
                .any(|source| source.root == source_root.as_path())
        });
        let retained = snapshot
            .as_slice()
            .iter()
            .find(|source| source.root == source_root.as_path())
            .expect("added source")
            .clone();
        assert_eq!(request.get(), 1);
        wait_for_event(&service, |event| {
            matches!(
                event,
                RegistryEvent::RequestApplied { request: applied, .. } if *applied == request
            )
        });
        wait_for_event(
            &service,
            |event| matches!(event, RegistryEvent::Persisted { revision } if revision.get() > 0),
        );
        service.remove_root(source_root.clone()).expect("remove");
        wait_for_event(
            &service,
            |event| matches!(event, RegistryEvent::Persisted { revision } if revision.get() > 0),
        );
        service.add_root(source_root.clone()).expect("readd");
        let readded = wait_for_snapshot(&service, |snapshot| {
            snapshot
                .as_slice()
                .iter()
                .any(|source| source.root == source_root.as_path())
        });
        let readded = readded
            .as_slice()
            .iter()
            .find(|source| source.root == source_root.as_path())
            .expect("readded source");
        assert_eq!(readded.id, retained.id);
        assert_eq!(
            readded.metadata_storage,
            SourceMetadataStorage::SourceFolder
        );

        let duplicate_request = service.add_root(source_root).expect("duplicate add");
        wait_for_event(&service, |event| {
            matches!(
                event,
                RegistryEvent::RequestApplied { request, .. } if *request == duplicate_request
            )
        });
        service.shutdown().expect("shutdown");
    }

    fn wait_for_snapshot(
        service: &SourceRegistryService,
        predicate: impl Fn(&SourceRegistrySnapshot) -> bool,
    ) -> SourceRegistrySnapshot {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match service.try_recv() {
                Ok(RegistryEvent::Snapshot { snapshot, .. }) if predicate(&snapshot) => {
                    return snapshot;
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) => {
                    assert!(Instant::now() < deadline, "timed out waiting for snapshot");
                    thread::sleep(Duration::from_millis(5));
                }
                Err(TryRecvError::Disconnected) => panic!("session event channel disconnected"),
            }
        }
    }
}
