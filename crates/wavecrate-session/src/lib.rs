#![deny(missing_docs)]
#![deny(warnings)]

//! GPUI-free ownership for the persisted source registry.
//!
//! The session worker owns the persistence profile and the library writer. UI code submits
//! canonical roots and consumes immutable registry snapshots; it never receives a database
//! connection or performs persistence I/O.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use thiserror::Error;
use wavecrate_library::app_dirs::{
    AppRootGuard, PersistenceProfileGuard, ProfileOwnershipError, WritableProfileGuard,
};
use wavecrate_library::sample_sources::{
    GlobalLibraryWriter, GlobalLibraryWriterQueueError, GlobalLibraryWriterStartError,
    GlobalLibraryWriterStatus, LibraryError, SampleSource, SourceRegistrySnapshot,
};

mod catalog;
mod preferences;

pub use catalog::{CatalogCommand, CatalogEvent, CatalogStatus, CatalogSubmitError};
use catalog::{CatalogService, CatalogSlot};

pub use preferences::{
    AudioOutputPreferences, PreferenceError, PreferenceEvent, PreferencePatch, PreferenceRequestId,
    PreferenceRevision, UI_PREFERENCES_SCHEMA_VERSION, UiPreferences,
};
use preferences::{PreferenceCoordinator, PreferenceOutcome, PreferenceSaveDeadline};

/// The non-live profile used by the GPUI application session.
pub const GPUI_DEVELOPMENT_PROFILE: &str = "gpui-development";

const SESSION_COMMAND_CAPACITY: usize = 32;
const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(10);
const IDLE_STATUS_CHECK_INTERVAL: Duration = Duration::from_millis(250);
// Keep retained-source lookups bounded so source mutations and preference commands always have
// room in the shared profile-owner queue.
const MAX_RETAINED_LOOKUPS_IN_FLIGHT: usize = 8;

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
    /// A preference document or preference patch was invalid.
    #[error("session preference error: {0}")]
    Preference(String),
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
    next_preference_request: AtomicU64,
    events: Receiver<RegistryEvent>,
    preference_events: Receiver<PreferenceEvent>,
    catalog: Arc<Mutex<CatalogSlot>>,
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

    /// Submit a typed preference patch to the shared session owner.
    pub fn patch_preferences(
        &self,
        patch: PreferencePatch,
    ) -> Result<PreferenceRequestId, SubmitError> {
        let request = PreferenceRequestId::from_raw(
            self.next_preference_request.fetch_add(1, Ordering::Relaxed),
        );
        self.submit_command(Command::PatchPreferences { request, patch })?;
        Ok(request)
    }

    /// Submit an explicit reset of the persisted preference document.
    pub fn reset_preferences(&self) -> Result<PreferenceRequestId, SubmitError> {
        self.patch_preferences(PreferencePatch::Reset)
    }

    /// Poll one session event without blocking the caller.
    pub fn try_recv(&self) -> Result<RegistryEvent, TryRecvError> {
        self.events.try_recv()
    }

    /// Poll one preference event without blocking the caller.
    pub fn try_recv_preference(&self) -> Result<PreferenceEvent, TryRecvError> {
        self.preference_events.try_recv()
    }

    /// Return the current lifecycle state of the independently-owned source catalog.
    pub fn catalog_status(&self) -> CatalogStatus {
        catalog::status(&self.catalog)
    }

    /// Submit one typed catalog command without waiting for catalog startup or worker I/O.
    pub fn submit_catalog(&self, command: CatalogCommand) -> Result<(), CatalogSubmitError> {
        catalog::submit(&self.catalog, command)
    }

    /// Poll one catalog event without waiting for startup or worker I/O.
    pub fn try_recv_catalog(&self) -> Result<CatalogEvent, TryRecvError> {
        catalog::try_recv_event(&self.catalog)
    }

    /// Close command admission, drain the final baseline/lookups/save, and synchronously join.
    pub fn shutdown(&mut self) -> Result<(), ShutdownError> {
        self.closed.store(true, Ordering::Release);
        catalog::close(&self.catalog);
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
        let (preference_event_tx, preference_event_rx) = mpsc::channel();
        let catalog = Arc::new(Mutex::new(CatalogSlot::loading()));
        let worker_catalog = Arc::clone(&catalog);
        let worker = thread::Builder::new()
            .name(String::from("wavecrate-source-session"))
            .spawn(move || {
                run_worker(
                    command_rx,
                    event_tx,
                    preference_event_tx,
                    app_root_override,
                    worker_catalog,
                )
            })
            .map_err(StartError::ThreadSpawn)?;
        Ok(Self {
            commands: Mutex::new(Some(command_tx)),
            closed: AtomicBool::new(false),
            next_request: AtomicU64::new(1),
            next_preference_request: AtomicU64::new(1),
            events: event_rx,
            preference_events: preference_event_rx,
            catalog,
            worker: Some(worker),
        })
    }

    fn submit(&self, command: impl FnOnce(RequestId) -> Command) -> Result<RequestId, SubmitError> {
        let request = RequestId(self.next_request.fetch_add(1, Ordering::Relaxed));
        self.submit_command(command(request))?;
        Ok(request)
    }

    fn submit_command(&self, command: Command) -> Result<(), SubmitError> {
        let commands = self
            .commands
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.closed.load(Ordering::Acquire) {
            return Err(SubmitError::Closed);
        }
        let sender = commands.as_ref().ok_or(SubmitError::Closed)?;
        sender.try_send(command).map_err(|error| match error {
            TrySendError::Full(_) => SubmitError::Full,
            TrySendError::Disconnected(_) => SubmitError::Closed,
        })?;
        Ok(())
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
    PatchPreferences {
        request: PreferenceRequestId,
        patch: PreferencePatch,
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

struct PreferenceSaveInFlight {
    revision: PreferenceRevision,
    receiver: Receiver<Result<(), LibraryError>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreferenceSaveHint {
    None,
    DebounceOutputVolume,
    Immediate,
}

impl PreferenceSaveHint {
    fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Immediate, _) | (_, Self::Immediate) => Self::Immediate,
            (Self::DebounceOutputVolume, _) | (_, Self::DebounceOutputVolume) => {
                Self::DebounceOutputVolume
            }
            (Self::None, Self::None) => Self::None,
        }
    }
}

struct Coordinator {
    loaded: bool,
    baseline: Vec<SampleSource>,
    current: Vec<SampleSource>,
    edits: Vec<Edit>,
    revision: RegistryRevision,
    durable_revision: RegistryRevision,
}

impl Coordinator {
    fn new() -> Self {
        Self {
            loaded: false,
            baseline: Vec::new(),
            current: Vec::new(),
            edits: Vec::new(),
            revision: RegistryRevision::default(),
            durable_revision: RegistryRevision::default(),
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
        self.current = self.baseline.clone();
        self.revision = RegistryRevision::default();
        self.durable_revision = RegistryRevision::default();
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
        self.revision != self.durable_revision
    }

    fn mark_persisted(&mut self) {
        self.durable_revision = self.revision;
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
    catalog: Option<Arc<CatalogService>>,
    load: Option<Receiver<Result<SourceRegistrySnapshot, LibraryError>>>,
    preference_load: Option<Receiver<Result<Option<String>, LibraryError>>>,
    pending_adds: HashMap<RequestId, PendingAdd>,
    save: Option<SaveInFlight>,
    preference_save: Option<PreferenceSaveInFlight>,
    preference_save_deadline: PreferenceSaveDeadline,
    force_save: bool,
    unavailable: Option<SessionError>,
    preference_unavailable: Option<SessionError>,
}

impl Persistence {
    fn start(
        app_root_override: Option<PathBuf>,
        catalog_slot: Arc<Mutex<CatalogSlot>>,
    ) -> (Self, Option<AppRootGuard>, PersistenceProfileGuard) {
        let profile_guard = PersistenceProfileGuard::named(GPUI_DEVELOPMENT_PROFILE);
        let mut persistence = Self {
            writable: None,
            writer: None,
            catalog: None,
            load: None,
            preference_load: None,
            pending_adds: HashMap::new(),
            save: None,
            preference_save: None,
            preference_save_deadline: PreferenceSaveDeadline::default(),
            force_save: false,
            unavailable: None,
            preference_unavailable: None,
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
        persistence.start_catalog(&catalog_slot);
        (persistence, root_guard, profile_guard)
    }

    fn start_catalog(&mut self, catalog_slot: &Arc<Mutex<CatalogSlot>>) {
        let Some(writable) = self.writable.as_ref() else {
            let reason = self
                .status_error()
                .map(|error| error.to_string())
                .unwrap_or_else(|| String::from("session profile owner unavailable"));
            catalog::mark_unavailable(catalog_slot, reason);
            return;
        };
        let service = match CatalogService::start(writable) {
            Ok(service) => Arc::new(service),
            Err(error) => {
                catalog::mark_unavailable(catalog_slot, error.to_string());
                return;
            }
        };
        if !catalog::install(catalog_slot, Arc::clone(&service)) {
            service.shutdown();
            return;
        }
        self.catalog = Some(service);
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
        if let Some(catalog) = self.catalog.take() {
            catalog.shutdown();
        }
        if let Some(mut writer) = self.writer.take() {
            let _ = writer.shutdown();
        }
        self.save = None;
        self.preference_load = None;
        self.preference_save = None;
        self.preference_save_deadline.clear();
        self.load = None;
        self.pending_adds.clear();
        self.preference_unavailable = None;
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
    preference_event_tx: mpsc::Sender<PreferenceEvent>,
    app_root_override: Option<PathBuf>,
    catalog_slot: Arc<Mutex<CatalogSlot>>,
) -> Result<(), SessionError> {
    let (mut persistence, _root_guard, _profile_guard) =
        Persistence::start(app_root_override, Arc::clone(&catalog_slot));
    let mut coordinator = Coordinator::new();
    let mut preference_coordinator = PreferenceCoordinator::new();
    let mut applied_requests = HashSet::new();
    let mut unfinalized_requests = HashSet::new();
    let mut applied_preference_requests = HashSet::new();
    let mut unfinalized_preference_requests = HashSet::new();
    let _ = event_tx.send(RegistryEvent::Loading {
        revision: coordinator.revision(),
    });
    let _ = preference_event_tx.send(PreferenceEvent::Loading {
        revision: preference_coordinator.revision(),
    });
    let mut reported_unavailable = false;
    let mut preference_reported_unavailable = false;
    if let Some(error) = persistence.status_error() {
        let revision = coordinator.revision();
        let preference_revision = preference_coordinator.revision();
        report_unavailable(
            &mut persistence,
            &mut coordinator,
            &event_tx,
            revision,
            error.clone(),
            &mut reported_unavailable,
            &mut unfinalized_requests,
        );
        report_preference_unavailable(
            &mut preference_coordinator,
            &preference_event_tx,
            preference_revision,
            error,
            &mut preference_reported_unavailable,
            &mut unfinalized_preference_requests,
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
        drive_preferences(
            &mut persistence,
            &mut preference_coordinator,
            &preference_event_tx,
            &mut preference_reported_unavailable,
            &mut applied_preference_requests,
            &mut unfinalized_preference_requests,
            false,
        );
        let command = if can_block_on_command(&persistence, &coordinator, &preference_coordinator) {
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
            catalog::close(&catalog_slot);
            let result = drain_shutdown(
                &mut persistence,
                &mut coordinator,
                &mut preference_coordinator,
                &event_tx,
                &preference_event_tx,
                &mut reported_unavailable,
                &mut preference_reported_unavailable,
                &mut applied_requests,
                &mut unfinalized_requests,
                &mut applied_preference_requests,
                &mut unfinalized_preference_requests,
            );
            persistence.shutdown();
            let _ = event_tx.send(RegistryEvent::Closed);
            let _ = preference_event_tx.send(PreferenceEvent::Closed);
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
        if let Some(request) = preference_command_request(&command) {
            unfinalized_preference_requests.insert(request);
            if let Some(error) = persistence.status_error() {
                let revision = preference_coordinator.revision();
                let source_revision = coordinator.revision();
                report_unavailable(
                    &mut persistence,
                    &mut coordinator,
                    &event_tx,
                    source_revision,
                    error.clone(),
                    &mut reported_unavailable,
                    &mut unfinalized_requests,
                );
                report_preference_unavailable(
                    &mut preference_coordinator,
                    &preference_event_tx,
                    revision,
                    error,
                    &mut preference_reported_unavailable,
                    &mut unfinalized_preference_requests,
                );
                continue;
            }
            let is_reset = matches!(
                &command,
                Command::PatchPreferences {
                    patch: PreferencePatch::Reset,
                    ..
                }
            );
            if let Some(error) = persistence.preference_unavailable.clone() {
                if !is_reset {
                    fail_preference_request(
                        &preference_event_tx,
                        &mut unfinalized_preference_requests,
                        request,
                        error,
                    );
                    continue;
                }
                persistence.preference_unavailable = None;
                preference_reported_unavailable = false;
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
            Command::PatchPreferences { request, patch } => {
                let output_volume_patch = matches!(&patch, PreferencePatch::SetOutputVolume(_));
                match preference_coordinator.apply_loaded_patch(request, patch) {
                    Ok(changed) if preference_coordinator.loaded() => {
                        if changed {
                            apply_preference_save_hint(
                                &mut persistence,
                                if output_volume_patch {
                                    PreferenceSaveHint::DebounceOutputVolume
                                } else {
                                    PreferenceSaveHint::Immediate
                                },
                            );
                            let _ = preference_event_tx.send(PreferenceEvent::Snapshot {
                                revision: preference_coordinator.revision(),
                                preferences: preference_coordinator.snapshot(),
                            });
                        }
                        announce_preference_request(
                            &preference_event_tx,
                            &mut applied_preference_requests,
                            &mut unfinalized_preference_requests,
                            request,
                            preference_coordinator.revision(),
                        );
                    }
                    Ok(_) => {}
                    Err(error) => fail_preference_request(
                        &preference_event_tx,
                        &mut unfinalized_preference_requests,
                        request,
                        SessionError::Preference(error.to_string()),
                    ),
                }
            }
            Command::Shutdown { acknowledgement } => {
                catalog::close(&catalog_slot);
                let result = drain_shutdown(
                    &mut persistence,
                    &mut coordinator,
                    &mut preference_coordinator,
                    &event_tx,
                    &preference_event_tx,
                    &mut reported_unavailable,
                    &mut preference_reported_unavailable,
                    &mut applied_requests,
                    &mut unfinalized_requests,
                    &mut applied_preference_requests,
                    &mut unfinalized_preference_requests,
                );
                persistence.shutdown();
                let _ = event_tx.send(RegistryEvent::Closed);
                let _ = preference_event_tx.send(PreferenceEvent::Closed);
                let _ = acknowledgement.send(result.clone().map_err(|error| error.clone()));
                return result;
            }
        }
    }
}

fn can_block_on_command(
    persistence: &Persistence,
    coordinator: &Coordinator,
    preference_coordinator: &PreferenceCoordinator,
) -> bool {
    persistence.unavailable.is_some()
        || (coordinator.loaded
            && persistence.load.is_none()
            && persistence.pending_adds.is_empty()
            && persistence.save.is_none()
            && (preference_coordinator.loaded() || persistence.preference_unavailable.is_some())
            && persistence.preference_load.is_none()
            && persistence.preference_save.is_none()
            && !persistence.force_save
            && !coordinator.is_dirty()
            && (persistence.preference_unavailable.is_some() || !preference_coordinator.is_dirty())
            && persistence.unavailable.is_none())
}

fn command_request(command: &Command) -> Option<RequestId> {
    match command {
        Command::AddRoot { request, .. } | Command::RemoveRoot { request, .. } => Some(*request),
        Command::PatchPreferences { .. } | Command::Shutdown { .. } => None,
    }
}

fn preference_command_request(command: &Command) -> Option<PreferenceRequestId> {
    match command {
        Command::PatchPreferences { request, .. } => Some(*request),
        Command::AddRoot { .. } | Command::RemoveRoot { .. } | Command::Shutdown { .. } => None,
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

fn drive_preferences(
    persistence: &mut Persistence,
    coordinator: &mut PreferenceCoordinator,
    event_tx: &mpsc::Sender<PreferenceEvent>,
    reported_unavailable: &mut bool,
    applied_requests: &mut HashSet<PreferenceRequestId>,
    unfinalized_requests: &mut HashSet<PreferenceRequestId>,
    bypass_volume_deadline: bool,
) {
    if let Some(error) = persistence.unavailable.clone() {
        report_preference_unavailable(
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
        report_preference_unavailable(
            coordinator,
            event_tx,
            coordinator.revision(),
            error,
            reported_unavailable,
            unfinalized_requests,
        );
        return;
    }
    if let Some(error) = persistence.preference_unavailable.clone() {
        report_preference_unavailable(
            coordinator,
            event_tx,
            coordinator.revision(),
            error,
            reported_unavailable,
            unfinalized_requests,
        );
        return;
    }
    queue_preference_load(persistence, coordinator);
    poll_preference_load(
        persistence,
        coordinator,
        event_tx,
        applied_requests,
        unfinalized_requests,
    );
    queue_preference_save(persistence, coordinator, bypass_volume_deadline);
    poll_preference_save(
        persistence,
        coordinator,
        event_tx,
        reported_unavailable,
        unfinalized_requests,
    );
    queue_preference_save(persistence, coordinator, bypass_volume_deadline);
    if let Some(error) = persistence.unavailable.clone() {
        report_preference_unavailable(
            coordinator,
            event_tx,
            coordinator.revision(),
            error,
            reported_unavailable,
            unfinalized_requests,
        );
    } else if let Some(error) = persistence.preference_unavailable.clone() {
        report_preference_unavailable(
            coordinator,
            event_tx,
            coordinator.revision(),
            error,
            reported_unavailable,
            unfinalized_requests,
        );
    }
}

fn apply_preference_save_hint(persistence: &mut Persistence, hint: PreferenceSaveHint) {
    match hint {
        PreferenceSaveHint::None => {}
        PreferenceSaveHint::DebounceOutputVolume => persistence
            .preference_save_deadline
            .defer_output_volume(Instant::now()),
        PreferenceSaveHint::Immediate => persistence.preference_save_deadline.clear(),
    }
}

fn queue_preference_load(persistence: &mut Persistence, coordinator: &PreferenceCoordinator) {
    if coordinator.loaded() || persistence.preference_load.is_some() || !persistence.available() {
        return;
    }
    let Some(writer) = persistence.writer.as_ref() else {
        return;
    };
    match writer.load_ui_preferences_payload() {
        Ok(receiver) => persistence.preference_load = Some(receiver),
        Err(GlobalLibraryWriterQueueError::Initializing | GlobalLibraryWriterQueueError::Full) => {}
        Err(error) => persistence.unavailable = Some(session_from_queue_error(error)),
    }
}

fn poll_preference_load(
    persistence: &mut Persistence,
    coordinator: &mut PreferenceCoordinator,
    event_tx: &mpsc::Sender<PreferenceEvent>,
    applied_requests: &mut HashSet<PreferenceRequestId>,
    unfinalized_requests: &mut HashSet<PreferenceRequestId>,
) {
    let Some(receiver) = persistence.preference_load.as_ref() else {
        return;
    };
    match receiver.try_recv() {
        Ok(Ok(None)) => {
            persistence.preference_load = None;
            let outcomes = coordinator.load_missing();
            let hint = emit_preference_snapshot_and_outcomes(
                coordinator,
                event_tx,
                applied_requests,
                unfinalized_requests,
                outcomes,
                true,
            );
            apply_preference_save_hint(persistence, hint);
        }
        Ok(Ok(Some(payload))) => {
            persistence.preference_load = None;
            match UiPreferences::from_payload(&payload) {
                Ok(preferences) => {
                    let outcomes = coordinator.load_valid(preferences);
                    let hint = emit_preference_snapshot_and_outcomes(
                        coordinator,
                        event_tx,
                        applied_requests,
                        unfinalized_requests,
                        outcomes,
                        true,
                    );
                    apply_preference_save_hint(persistence, hint);
                }
                Err(error) => {
                    let outcomes = coordinator.load_invalid();
                    let _ = event_tx.send(PreferenceEvent::LoadFailed {
                        error: SessionError::Preference(error.to_string()),
                    });
                    let hint = emit_preference_outcomes(
                        coordinator,
                        event_tx,
                        applied_requests,
                        unfinalized_requests,
                        outcomes,
                    );
                    apply_preference_save_hint(persistence, hint);
                    if coordinator.valid() {
                        let _ = event_tx.send(PreferenceEvent::Snapshot {
                            revision: coordinator.revision(),
                            preferences: coordinator.snapshot(),
                        });
                    }
                }
            }
        }
        Ok(Err(error)) if is_invalid_preference_payload_error(&error) => {
            persistence.preference_load = None;
            let outcomes = coordinator.load_invalid();
            let _ = event_tx.send(PreferenceEvent::LoadFailed {
                error: SessionError::Preference(error.to_string()),
            });
            let hint = emit_preference_outcomes(
                coordinator,
                event_tx,
                applied_requests,
                unfinalized_requests,
                outcomes,
            );
            apply_preference_save_hint(persistence, hint);
            if coordinator.valid() {
                let _ = event_tx.send(PreferenceEvent::Snapshot {
                    revision: coordinator.revision(),
                    preferences: coordinator.snapshot(),
                });
            }
        }
        Ok(Err(error)) => {
            persistence.preference_load = None;
            record_preference_operation_failure(persistence, error);
        }
        Err(TryRecvError::Empty) => {}
        Err(TryRecvError::Disconnected) => {
            persistence.preference_load = None;
            persistence.unavailable = Some(SessionError::Persistence(String::from(
                "UI preferences load receiver disconnected",
            )));
        }
    }
}

fn queue_preference_save(
    persistence: &mut Persistence,
    coordinator: &PreferenceCoordinator,
    bypass_volume_deadline: bool,
) {
    if !coordinator.loaded()
        || !coordinator.valid()
        || persistence.preference_load.is_some()
        || persistence.preference_save.is_some()
        || persistence.preference_unavailable.is_some()
        || !persistence.available()
        || !coordinator.is_dirty()
    {
        return;
    }
    if !persistence
        .preference_save_deadline
        .permits_save(Instant::now(), bypass_volume_deadline)
    {
        return;
    }
    let Some(writer) = persistence.writer.as_ref() else {
        return;
    };
    let payload = match coordinator.snapshot().to_payload() {
        Ok(payload) => payload,
        Err(error) => {
            persistence.preference_unavailable = Some(SessionError::Preference(error.to_string()));
            return;
        }
    };
    match writer.replace_ui_preferences_payload(payload) {
        Ok(receiver) => {
            persistence.preference_save = Some(PreferenceSaveInFlight {
                revision: coordinator.revision(),
                receiver,
            });
            persistence.preference_save_deadline.clear();
        }
        Err(GlobalLibraryWriterQueueError::Initializing | GlobalLibraryWriterQueueError::Full) => {}
        Err(error @ GlobalLibraryWriterQueueError::PayloadTooLarge { .. }) => {
            persistence.preference_unavailable = Some(SessionError::Preference(error.to_string()));
        }
        Err(error) => persistence.unavailable = Some(session_from_queue_error(error)),
    }
}

fn poll_preference_save(
    persistence: &mut Persistence,
    coordinator: &mut PreferenceCoordinator,
    event_tx: &mpsc::Sender<PreferenceEvent>,
    reported_unavailable: &mut bool,
    unfinalized_requests: &mut HashSet<PreferenceRequestId>,
) {
    let Some(save) = persistence.preference_save.as_ref() else {
        return;
    };
    match save.receiver.try_recv() {
        Ok(Ok(())) => {
            let revision = save.revision;
            persistence.preference_save = None;
            if coordinator.accept_save_ack(revision) {
                let _ = event_tx.send(PreferenceEvent::Persisted { revision });
            }
        }
        Ok(Err(error)) => {
            persistence.preference_save = None;
            record_preference_operation_failure(persistence, error);
        }
        Err(TryRecvError::Empty) => {}
        Err(TryRecvError::Disconnected) => {
            persistence.preference_save = None;
            persistence.unavailable = Some(SessionError::Persistence(String::from(
                "UI preferences save receiver disconnected",
            )));
        }
    }
    if let Some(error) = persistence.unavailable.clone() {
        report_preference_unavailable(
            coordinator,
            event_tx,
            coordinator.revision(),
            error,
            reported_unavailable,
            unfinalized_requests,
        );
    }
}

fn is_invalid_preference_payload_error(error: &LibraryError) -> bool {
    matches!(
        error,
        LibraryError::MetadataTooLarge { .. } | LibraryError::MetadataInvalidUtf8 { .. }
    )
}

fn record_preference_operation_failure(persistence: &mut Persistence, error: LibraryError) {
    if let Some(shared_error) = shared_preference_failure(persistence, &error) {
        persistence.unavailable = Some(shared_error);
    } else {
        persistence.preference_unavailable = Some(SessionError::Preference(error.to_string()));
    }
}

fn shared_preference_failure(
    persistence: &Persistence,
    error: &LibraryError,
) -> Option<SessionError> {
    persistence.status_error().or_else(|| {
        matches!(
            error,
            LibraryError::ProfileOwnership(_) | LibraryError::ProfileOwnershipChanged { .. }
        )
        .then(|| SessionError::Persistence(error.to_string()))
    })
}

fn emit_preference_snapshot_and_outcomes(
    coordinator: &PreferenceCoordinator,
    event_tx: &mpsc::Sender<PreferenceEvent>,
    applied_requests: &mut HashSet<PreferenceRequestId>,
    unfinalized_requests: &mut HashSet<PreferenceRequestId>,
    outcomes: Vec<PreferenceOutcome>,
    emit_persisted_baseline: bool,
) -> PreferenceSaveHint {
    let _ = event_tx.send(PreferenceEvent::Snapshot {
        revision: coordinator.revision(),
        preferences: coordinator.snapshot(),
    });
    if emit_persisted_baseline {
        let _ = event_tx.send(PreferenceEvent::Persisted {
            revision: PreferenceRevision::default(),
        });
    }
    emit_preference_outcomes(
        coordinator,
        event_tx,
        applied_requests,
        unfinalized_requests,
        outcomes,
    )
}

fn emit_preference_outcomes(
    coordinator: &PreferenceCoordinator,
    event_tx: &mpsc::Sender<PreferenceEvent>,
    applied_requests: &mut HashSet<PreferenceRequestId>,
    unfinalized_requests: &mut HashSet<PreferenceRequestId>,
    outcomes: Vec<PreferenceOutcome>,
) -> PreferenceSaveHint {
    let mut hint = PreferenceSaveHint::None;
    for outcome in outcomes {
        match outcome.result {
            Ok(changed) => {
                if changed {
                    hint = hint.merge(if outcome.output_volume_changed {
                        PreferenceSaveHint::DebounceOutputVolume
                    } else {
                        PreferenceSaveHint::Immediate
                    });
                }
                announce_preference_request(
                    event_tx,
                    applied_requests,
                    unfinalized_requests,
                    outcome.request,
                    coordinator.revision(),
                );
            }
            Err(error) => fail_preference_request(
                event_tx,
                unfinalized_requests,
                outcome.request,
                SessionError::Preference(error.to_string()),
            ),
        }
    }
    hint
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
    let in_flight = persistence
        .pending_adds
        .values()
        .filter(|pending| pending.receiver.is_some())
        .count();
    let mut available = MAX_RETAINED_LOOKUPS_IN_FLIGHT.saturating_sub(in_flight);
    if available == 0 {
        return;
    }
    let mut fatal_error = None;
    for pending in persistence.pending_adds.values_mut() {
        if pending.receiver.is_some() {
            continue;
        }
        if available == 0 {
            break;
        }
        match writer.lookup_retained_source_for_root(pending.root.clone().into_path_buf()) {
            Ok(receiver) => {
                pending.receiver = Some(receiver);
                available -= 1;
            }
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

fn report_preference_unavailable(
    coordinator: &mut PreferenceCoordinator,
    event_tx: &mpsc::Sender<PreferenceEvent>,
    revision: PreferenceRevision,
    error: SessionError,
    reported: &mut bool,
    unfinalized_requests: &mut HashSet<PreferenceRequestId>,
) {
    let mut failed_requests: Vec<_> = unfinalized_requests.drain().collect();
    failed_requests.sort_by_key(|request| request.get());
    for request in failed_requests {
        let _ = event_tx.send(PreferenceEvent::RequestFailed {
            request,
            error: error.clone(),
        });
    }
    if !*reported {
        let _ = event_tx.send(PreferenceEvent::PersistenceUnavailable { revision, error });
        *reported = true;
    }
    coordinator.clear_pending();
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

fn fail_preference_request(
    event_tx: &mpsc::Sender<PreferenceEvent>,
    unfinalized_requests: &mut HashSet<PreferenceRequestId>,
    request: PreferenceRequestId,
    error: SessionError,
) {
    if unfinalized_requests.remove(&request) {
        let _ = event_tx.send(PreferenceEvent::RequestFailed { request, error });
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

fn announce_preference_request(
    event_tx: &mpsc::Sender<PreferenceEvent>,
    applied_requests: &mut HashSet<PreferenceRequestId>,
    unfinalized_requests: &mut HashSet<PreferenceRequestId>,
    request: PreferenceRequestId,
    revision: PreferenceRevision,
) {
    unfinalized_requests.remove(&request);
    if applied_requests.insert(request) {
        let _ = event_tx.send(PreferenceEvent::RequestApplied { request, revision });
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
    preference_coordinator: &mut PreferenceCoordinator,
    event_tx: &mpsc::Sender<RegistryEvent>,
    preference_event_tx: &mpsc::Sender<PreferenceEvent>,
    reported_unavailable: &mut bool,
    preference_reported_unavailable: &mut bool,
    applied_requests: &mut HashSet<RequestId>,
    unfinalized_requests: &mut HashSet<RequestId>,
    applied_preference_requests: &mut HashSet<PreferenceRequestId>,
    unfinalized_preference_requests: &mut HashSet<PreferenceRequestId>,
) -> Result<(), SessionError> {
    loop {
        if let Some(error) = persistence.status_error() {
            let preference_revision = preference_coordinator.revision();
            report_unavailable(
                persistence,
                coordinator,
                event_tx,
                coordinator.revision(),
                error.clone(),
                reported_unavailable,
                unfinalized_requests,
            );
            report_preference_unavailable(
                preference_coordinator,
                preference_event_tx,
                preference_revision,
                error.clone(),
                preference_reported_unavailable,
                unfinalized_preference_requests,
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
        drive_preferences(
            persistence,
            preference_coordinator,
            preference_event_tx,
            preference_reported_unavailable,
            applied_preference_requests,
            unfinalized_preference_requests,
            true,
        );
        if persistence.unavailable.is_some() {
            return Err(persistence
                .unavailable
                .clone()
                .expect("unavailable state retained"));
        }
        if coordinator.loaded
            && (preference_coordinator.loaded() || persistence.preference_unavailable.is_some())
            && persistence.load.is_none()
            && persistence.preference_load.is_none()
            && persistence.pending_adds.is_empty()
            && persistence.save.is_none()
            && persistence.preference_save.is_none()
            && !persistence.force_save
            && !coordinator.is_dirty()
            && (persistence.preference_unavailable.is_some() || !preference_coordinator.is_dirty())
        {
            return persistence
                .preference_unavailable
                .clone()
                .map_or(Ok(()), Err);
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
    use wavecrate_library::app_dirs::{
        AppRootGuard, ConfigBaseGuard, PROFILE_DIR_NAME, ProfileOwnershipError,
        WritableProfileGuard,
    };
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

    fn wait_for_preference_event(
        service: &SourceRegistryService,
        predicate: impl Fn(&PreferenceEvent) -> bool,
    ) -> PreferenceEvent {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match service.try_recv_preference() {
                Ok(event) if predicate(&event) => return event,
                Ok(_) => {}
                Err(TryRecvError::Empty) => {
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting for preference event"
                    );
                    thread::sleep(Duration::from_millis(5));
                }
                Err(TryRecvError::Disconnected) => {
                    panic!("preference event channel disconnected")
                }
            }
        }
    }

    fn wait_for_preference_snapshot(
        service: &SourceRegistryService,
        predicate: impl Fn(&UiPreferences) -> bool,
    ) -> UiPreferences {
        match wait_for_preference_event(
            service,
            |event| matches!(event, PreferenceEvent::Snapshot { preferences, .. } if predicate(preferences)),
        ) {
            PreferenceEvent::Snapshot { preferences, .. } => preferences,
            _ => unreachable!("snapshot predicate returned a non-snapshot event"),
        }
    }

    fn wait_for_catalog_ready(service: &SourceRegistryService) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match service.catalog_status() {
                CatalogStatus::Ready => return,
                CatalogStatus::Loading => {
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting for catalog startup"
                    );
                    thread::sleep(Duration::from_millis(5));
                }
                CatalogStatus::Unavailable { reason } => {
                    panic!("catalog startup failed: {reason}");
                }
                CatalogStatus::Closed => panic!("catalog closed during startup"),
            }
        }
    }

    fn wait_for_catalog_event(
        service: &SourceRegistryService,
        predicate: impl Fn(&CatalogEvent) -> bool,
    ) -> CatalogEvent {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match service.try_recv_catalog() {
                Ok(event) if predicate(&event) => return event,
                Ok(_) => {}
                Err(TryRecvError::Empty) => {
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting for catalog event"
                    );
                    thread::sleep(Duration::from_millis(5));
                }
                Err(TryRecvError::Disconnected) => {
                    panic!("catalog event channel disconnected")
                }
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
    fn add_remove_reversion_requires_persisting_newer_revision() {
        let temp = TempDir::new().expect("temp");
        let source_root = root(&temp, "reversion");
        let mut coordinator = Coordinator::new();
        coordinator
            .load(SourceRegistrySnapshot::new(Vec::new()))
            .expect("load");
        coordinator.add_edit(RequestId(1), source_root.clone());
        coordinator
            .resolve_add(RequestId(1), source(&source_root, "reversion-id"))
            .expect("resolve add");
        let add_revision = coordinator.revision();
        assert_eq!(add_revision.get(), 1);
        coordinator.remove_edit(RequestId(2), source_root);
        assert!(coordinator.rebuild().expect("rebuild remove"));
        assert!(coordinator.current.is_empty());
        assert_eq!(coordinator.revision().get(), 2);
        assert!(coordinator.is_dirty());
        assert!(coordinator.accept_save_ack(coordinator.revision()));
        assert!(!coordinator.is_dirty());
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
    fn catalog_shares_session_profile_owner_and_releases_after_shutdown() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile_root = profile_root(&temp);
        let mut service =
            SourceRegistryService::start_inner(Some(profile_root.clone())).expect("service start");
        wait_for_catalog_ready(&service);

        let app_root_guard = AppRootGuard::set(profile_root.clone()).expect("app root");
        assert!(matches!(
            WritableProfileGuard::acquire_current(),
            Err(ProfileOwnershipError::ProfileOwnedByAnotherProcess { .. })
        ));
        drop(app_root_guard);

        service.shutdown().expect("session shutdown");

        let app_root_guard = AppRootGuard::set(profile_root).expect("app root after shutdown");
        let replacement =
            WritableProfileGuard::acquire_current().expect("profile ownership released");
        drop(replacement);
        drop(app_root_guard);
    }

    #[test]
    fn catalog_audit_does_not_block_preference_persistence() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile_root = profile_root(&temp);
        let source_root = temp.path().join("catalog-source");
        for index in 0..256 {
            fs::create_dir_all(source_root.join(format!("folder-{index}/nested")))
                .expect("source fixture directory");
        }
        let source_root = fs::canonicalize(source_root).expect("canonical source root");
        let source_id = SourceId::from_string("session-catalog");
        let mut service =
            SourceRegistryService::start_inner(Some(profile_root)).expect("service start");
        wait_for_catalog_ready(&service);
        wait_for_preference_snapshot(&service, |preferences| {
            preferences == &UiPreferences::default()
        });

        service
            .submit_catalog(CatalogCommand::Register {
                source_id: source_id.clone(),
                canonical_root: source_root,
                lifecycle_generation: 1,
            })
            .expect("catalog registration");
        wait_for_catalog_event(
            &service,
            |event| matches!(event, CatalogEvent::Registered { source_id: registered, .. } if registered == &source_id),
        );
        service
            .submit_catalog(CatalogCommand::RequestAudit {
                source_id: source_id.clone(),
                lifecycle_generation: 1,
            })
            .expect("catalog audit");

        let preference_request = service
            .patch_preferences(PreferencePatch::SetLoopEnabled(true))
            .expect("preference patch during audit");
        wait_for_preference_event(&service, |event| {
            matches!(
                event,
                PreferenceEvent::RequestApplied { request, .. } if *request == preference_request
            )
        });
        wait_for_preference_event(
            &service,
            |event| matches!(event, PreferenceEvent::Persisted { revision } if revision.get() > 0),
        );
        wait_for_catalog_event(
            &service,
            |event| matches!(event, CatalogEvent::AuditCommitted { snapshot } if snapshot.source_id == source_id),
        );
        service.shutdown().expect("session shutdown");
    }

    #[test]
    fn immediate_shutdown_closes_catalog_admission() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile_root = profile_root(&temp);
        let mut service =
            SourceRegistryService::start_inner(Some(profile_root)).expect("service start");
        service.shutdown().expect("immediate shutdown");

        assert_eq!(service.catalog_status(), CatalogStatus::Closed);
        assert_eq!(
            service.submit_catalog(CatalogCommand::ReadSnapshot {
                source_id: SourceId::from_string("closed-catalog"),
                expected_revision: None,
            }),
            Err(CatalogSubmitError::Closed)
        );
        assert!(matches!(
            service.try_recv_catalog(),
            Err(TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn preferences_round_trip_with_missing_theme_source_path() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile_root = profile_root(&temp);
        let mut service =
            SourceRegistryService::start_inner(Some(profile_root.clone())).expect("service start");
        wait_for_preference_snapshot(&service, |preferences| {
            preferences == &UiPreferences::default()
        });

        let mut theme = wavecrate_design::Theme::default();
        theme.name = String::from("Persisted theme");
        theme.motion.enabled = false;
        let source_path = temp.path().join("missing custom theme.json");
        let replace_request = service
            .patch_preferences(PreferencePatch::ReplaceTheme {
                theme: theme.clone(),
                source_path: Some(source_path.clone()),
            })
            .expect("replace theme");
        wait_for_preference_event(&service, |event| {
            matches!(
                event,
                PreferenceEvent::RequestApplied { request, .. } if *request == replace_request
            )
        });
        wait_for_preference_event(
            &service,
            |event| matches!(event, PreferenceEvent::Persisted { revision } if revision.get() > 0),
        );

        let motion_request = service
            .patch_preferences(PreferencePatch::SetMotionEnabled(true))
            .expect("motion patch");
        wait_for_preference_event(&service, |event| {
            matches!(
                event,
                PreferenceEvent::RequestApplied { request, .. } if *request == motion_request
            )
        });
        let autoplay_request = service
            .patch_preferences(PreferencePatch::SetAutoplaySelection(false))
            .expect("autoplay patch");
        wait_for_preference_event(&service, |event| {
            matches!(
                event,
                PreferenceEvent::RequestApplied { request, .. } if *request == autoplay_request
            )
        });
        let loop_request = service
            .patch_preferences(PreferencePatch::SetLoopEnabled(true))
            .expect("loop patch");
        wait_for_preference_event(&service, |event| {
            matches!(
                event,
                PreferenceEvent::RequestApplied { request, .. } if *request == loop_request
            )
        });
        wait_for_preference_event(
            &service,
            |event| matches!(event, PreferenceEvent::Persisted { revision } if revision.get() > 2),
        );
        service.shutdown().expect("preference shutdown");

        let mut reopened =
            SourceRegistryService::start_inner(Some(profile_root)).expect("reopen service");
        let restored = wait_for_preference_snapshot(&reopened, |preferences| {
            preferences.theme.name == "Persisted theme"
                && preferences.theme.motion.enabled
                && preferences.theme_source_path.as_deref() == Some(source_path.as_path())
                && !preferences.autoplay_selection
                && preferences.loop_enabled
        });
        let mut expected_theme = theme;
        expected_theme.motion.enabled = true;
        assert_eq!(
            restored.theme,
            wavecrate_design::validate_theme(expected_theme).expect("theme valid")
        );
        assert_eq!(restored.theme_source_path, Some(source_path));
        reopened.shutdown().expect("reopen preference shutdown");
    }

    #[test]
    fn immediate_shutdown_persists_latest_output_volume() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile_root = profile_root(&temp);
        let mut service =
            SourceRegistryService::start_inner(Some(profile_root.clone())).expect("service start");
        wait_for_preference_snapshot(&service, |preferences| {
            preferences == &UiPreferences::default()
        });

        service
            .patch_preferences(PreferencePatch::SetOutputVolume(0.25))
            .expect("first volume patch");
        service
            .patch_preferences(PreferencePatch::SetOutputVolume(0.73))
            .expect("latest volume patch");
        service.shutdown().expect("immediate shutdown");

        let mut reopened =
            SourceRegistryService::start_inner(Some(profile_root)).expect("reopen service");
        let restored = wait_for_preference_snapshot(&reopened, |preferences| {
            (preferences.output_volume - 0.73).abs() < f32::EPSILON
        });
        assert!((restored.output_volume - 0.73).abs() < f32::EPSILON);
        reopened.shutdown().expect("reopen shutdown");
    }

    #[test]
    fn preference_reversion_persists_latest_revision() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile_root = profile_root(&temp);
        let mut service =
            SourceRegistryService::start_inner(Some(profile_root.clone())).expect("service start");
        wait_for_preference_snapshot(&service, |preferences| {
            preferences == &UiPreferences::default()
        });
        let disable_motion = service
            .patch_preferences(PreferencePatch::SetMotionEnabled(false))
            .expect("disable motion");
        let restore_motion = service
            .patch_preferences(PreferencePatch::SetMotionEnabled(true))
            .expect("restore motion");
        wait_for_preference_event(&service, |event| {
            matches!(
                event,
                PreferenceEvent::RequestApplied { request, .. } if *request == disable_motion
            )
        });
        wait_for_preference_event(&service, |event| {
            matches!(
                event,
                PreferenceEvent::RequestApplied { request, .. } if *request == restore_motion
            )
        });
        wait_for_preference_event(
            &service,
            |event| matches!(event, PreferenceEvent::Persisted { revision } if revision.get() > 1),
        );
        service.shutdown().expect("reversion shutdown");

        let mut reopened =
            SourceRegistryService::start_inner(Some(profile_root)).expect("reopen service");
        wait_for_preference_snapshot(&reopened, |preferences| {
            preferences == &UiPreferences::default()
        });
        reopened.shutdown().expect("reopen shutdown");
    }

    #[test]
    fn malformed_preferences_remain_untouched_until_explicit_reset() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile_root = profile_root(&temp);
        let mut service =
            SourceRegistryService::start_inner(Some(profile_root.clone())).expect("service start");
        wait_for_preference_snapshot(&service, |preferences| {
            preferences == &UiPreferences::default()
        });
        service.shutdown().expect("initial shutdown");

        let app_root_guard = AppRootGuard::set(profile_root.clone()).expect("app root");
        let connection = wavecrate_library::sample_sources::library::open_connection()
            .expect("open preference fixture");
        connection
            .execute(
                "INSERT INTO metadata (key, value) VALUES ('ui_preferences_v1', 'malformed')
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [],
            )
            .expect("write malformed preference");
        drop(connection);
        drop(app_root_guard);

        let mut malformed = SourceRegistryService::start_inner(Some(profile_root.clone()))
            .expect("reopen malformed service");
        wait_for_preference_event(&malformed, |event| {
            matches!(event, PreferenceEvent::LoadFailed { .. })
        });
        let rejected = malformed
            .patch_preferences(PreferencePatch::SetLoopEnabled(true))
            .expect("submit rejected preference");
        wait_for_preference_event(&malformed, |event| {
            matches!(
                event,
                PreferenceEvent::RequestFailed { request, error }
                    if *request == rejected && matches!(error, SessionError::Preference(_))
            )
        });
        let reset = malformed.reset_preferences().expect("reset preference");
        wait_for_preference_snapshot(&malformed, |preferences| {
            preferences == &UiPreferences::default()
        });
        wait_for_preference_event(&malformed, |event| {
            matches!(
                event,
                PreferenceEvent::RequestApplied { request, .. } if *request == reset
            )
        });
        wait_for_preference_event(
            &malformed,
            |event| matches!(event, PreferenceEvent::Persisted { revision } if revision.get() > 0),
        );
        malformed.shutdown().expect("reset shutdown");

        let app_root_guard = AppRootGuard::set(profile_root).expect("app root reopen");
        let connection = wavecrate_library::sample_sources::library::open_connection()
            .expect("open reset preference");
        let value: String = connection
            .query_row(
                "SELECT value FROM metadata WHERE key = 'ui_preferences_v1'",
                [],
                |row| row.get(0),
            )
            .expect("preference row");
        assert_ne!(value, "malformed");
        drop(connection);
        drop(app_root_guard);
    }

    #[test]
    fn preference_save_failure_keeps_source_registry_available() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile_root = profile_root(&temp);
        let mut initial =
            SourceRegistryService::start_inner(Some(profile_root.clone())).expect("service start");
        wait_for_event(&initial, |event| {
            matches!(event, RegistryEvent::Snapshot { .. })
        });
        wait_for_preference_snapshot(&initial, |preferences| {
            preferences == &UiPreferences::default()
        });
        initial.shutdown().expect("initial shutdown");

        let app_root_guard = AppRootGuard::set(profile_root.clone()).expect("app root");
        let connection = wavecrate_library::sample_sources::library::open_connection()
            .expect("open preference fixture");
        let payload = UiPreferences::default()
            .to_payload()
            .expect("default preference payload");
        connection
            .execute(
                "INSERT INTO metadata (key, value) VALUES ('ui_preferences_v1', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [payload.as_str()],
            )
            .expect("write preference fixture");
        connection
            .execute_batch(
                "CREATE TRIGGER fail_ui_preferences_session_update
                 BEFORE UPDATE OF value ON metadata
                 WHEN OLD.key = 'ui_preferences_v1'
                 BEGIN SELECT RAISE(FAIL, 'session preference failure fixture'); END;",
            )
            .expect("preference failure trigger");
        drop(connection);
        drop(app_root_guard);

        let mut service =
            SourceRegistryService::start_inner(Some(profile_root)).expect("reopen service");
        wait_for_event(&service, |event| {
            matches!(event, RegistryEvent::Snapshot { .. })
        });
        wait_for_preference_snapshot(&service, |preferences| {
            preferences == &UiPreferences::default()
        });
        let preference_request = service
            .patch_preferences(PreferencePatch::SetLoopEnabled(true))
            .expect("preference patch");
        wait_for_preference_event(&service, |event| {
            matches!(
                event,
                PreferenceEvent::RequestApplied { request, .. } if *request == preference_request
            )
        });
        wait_for_preference_event(
            &service,
            |event| matches!(event, PreferenceEvent::PersistenceUnavailable { error, .. } if matches!(error, SessionError::Preference(_))),
        );

        let source_root = root(&temp, "source-after-preference-failure");
        let add_request = service.add_root(source_root.clone()).expect("source add");
        wait_for_event(&service, |event| {
            matches!(
                event,
                RegistryEvent::RequestApplied { request, .. } if *request == add_request
            )
        });
        let remove_request = service.remove_root(source_root).expect("source remove");
        wait_for_event(&service, |event| {
            matches!(
                event,
                RegistryEvent::RequestApplied { request, .. } if *request == remove_request
            )
        });
        assert!(matches!(
            service.shutdown(),
            Err(ShutdownError::Session(SessionError::Preference(_)))
        ));
    }

    #[test]
    fn preference_save_is_not_starved_by_retained_lookup_backlog() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile_root = profile_root(&temp);
        let mut service =
            SourceRegistryService::start_inner(Some(profile_root)).expect("service start");
        wait_for_event(&service, |event| {
            matches!(event, RegistryEvent::Snapshot { .. })
        });
        wait_for_preference_snapshot(&service, |preferences| {
            preferences == &UiPreferences::default()
        });

        for index in 0..128 {
            let source_root = root(&temp, &format!("lookup-backlog-{index}"));
            loop {
                match service.add_root(source_root.clone()) {
                    Ok(_) => break,
                    Err(SubmitError::Full) => thread::sleep(Duration::from_millis(1)),
                    Err(SubmitError::Closed) => panic!("session closed while queueing lookups"),
                }
            }
        }

        let preference_request = loop {
            match service.patch_preferences(PreferencePatch::SetLoopEnabled(true)) {
                Ok(request) => break request,
                Err(SubmitError::Full) => thread::sleep(Duration::from_millis(1)),
                Err(SubmitError::Closed) => panic!("session closed while queueing preference"),
            }
        };
        wait_for_preference_event(&service, |event| {
            matches!(
                event,
                PreferenceEvent::RequestApplied { request, .. } if *request == preference_request
            )
        });
        wait_for_preference_event(
            &service,
            |event| matches!(event, PreferenceEvent::Persisted { revision } if revision.get() > 0),
        );
        service.shutdown().expect("lookup backlog shutdown");
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
        service
            .patch_preferences(PreferencePatch::SetLoopEnabled(true))
            .expect("preference patch");
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
            catalog: None,
            load: None,
            preference_load: None,
            pending_adds: HashMap::new(),
            save: None,
            preference_save: None,
            preference_save_deadline: PreferenceSaveDeadline::default(),
            force_save: false,
            unavailable: None,
            preference_unavailable: None,
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
    fn unloaded_preference_failure_is_quiescent_for_shutdown() {
        let mut coordinator = Coordinator::new();
        coordinator
            .load(SourceRegistrySnapshot::new(Vec::new()))
            .expect("load source coordinator");
        let mut preference_coordinator = PreferenceCoordinator::new();
        let mut persistence = Persistence {
            writable: None,
            writer: None,
            catalog: None,
            load: None,
            preference_load: None,
            pending_adds: HashMap::new(),
            save: None,
            preference_save: None,
            preference_save_deadline: PreferenceSaveDeadline::default(),
            force_save: false,
            unavailable: None,
            preference_unavailable: Some(SessionError::Preference(String::from(
                "preference load query failed",
            ))),
        };
        assert!(can_block_on_command(
            &persistence,
            &coordinator,
            &preference_coordinator
        ));
        let (event_tx, _event_rx) = mpsc::channel();
        let (preference_event_tx, preference_event_rx) = mpsc::channel();
        let mut reported = false;
        let mut preference_reported = false;
        let mut applied = HashSet::new();
        let mut unfinalized = HashSet::new();
        let mut applied_preferences = HashSet::new();
        let mut unfinalized_preferences = HashSet::new();
        let result = drain_shutdown(
            &mut persistence,
            &mut coordinator,
            &mut preference_coordinator,
            &event_tx,
            &preference_event_tx,
            &mut reported,
            &mut preference_reported,
            &mut applied,
            &mut unfinalized,
            &mut applied_preferences,
            &mut unfinalized_preferences,
        );
        assert!(matches!(
            result,
            Err(SessionError::Preference(message)) if message == "preference load query failed"
        ));
        assert!(matches!(
            preference_event_rx.try_recv(),
            Ok(PreferenceEvent::PersistenceUnavailable { error: SessionError::Preference(message), .. })
                if message == "preference load query failed"
        ));
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
