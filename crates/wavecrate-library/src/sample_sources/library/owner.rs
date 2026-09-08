//! Profile-owned asynchronous ownership for the global library database.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use thiserror::Error;

use super::connection::LibraryDatabase;
use super::error::LibraryError;
use super::tag_vocabulary::{
    ProfileTagCategory, ProfileTagVocabularyInputError, ProfileTagVocabularyLookup,
    ProfileTagVocabularyPage, ProfileTagVocabularyPageCursor, ProfileTagVocabularySetReceipt,
    validate_lookup_labels, validate_page_inputs, validate_set_input,
};
use crate::app_dirs::{ProfileOwnershipError, WritableProfileGuard};
use crate::sample_sources::{SampleSource, UI_PREFERENCES_MAX_BYTES};

/// Maximum number of global-library commands waiting for the owner thread.
pub const GLOBAL_LIBRARY_WRITER_COMMAND_CAPACITY: usize = 32;

const OWNER_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(25);

/// An immutable, owned snapshot of the configured source registry.
#[derive(Clone, Debug)]
pub struct SourceRegistrySnapshot {
    sources: Arc<[SampleSource]>,
}

impl SourceRegistrySnapshot {
    /// Own a source registry snapshot without retaining a caller-owned mutable collection.
    pub fn new(sources: Vec<SampleSource>) -> Self {
        Self {
            sources: Arc::from(sources.into_boxed_slice()),
        }
    }

    /// Borrow the sources in this immutable snapshot.
    pub fn as_slice(&self) -> &[SampleSource] {
        &self.sources
    }

    /// Return the number of sources in this snapshot.
    pub fn len(&self) -> usize {
        self.sources.len()
    }

    /// Return whether this snapshot contains no sources.
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    pub(super) fn from_sources(sources: Vec<SampleSource>) -> Self {
        Self::new(sources)
    }
}

impl From<Vec<SampleSource>> for SourceRegistrySnapshot {
    fn from(sources: Vec<SampleSource>) -> Self {
        Self::new(sources)
    }
}

/// Lifecycle state visible at the global-library writer boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GlobalLibraryWriterStatus {
    /// The owner thread has not completed profile-owned database initialization.
    Initializing,
    /// The owner has one initialized writable library database connection.
    Available,
    /// Initialization failed and this participant will not admit commands.
    Unavailable {
        /// Reason that the participant is unavailable.
        reason: GlobalLibraryWriterUnavailable,
    },
    /// Shutdown has stopped new command admission and is draining accepted commands.
    Closing,
    /// The writer has closed its library database connection and worker thread.
    Closed,
}

/// Fail-closed reasons retained by the global-library owner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GlobalLibraryWriterUnavailable {
    /// The acquired profile root or lock entry no longer names the owned object.
    ProfileOwnershipChanged {
        /// Profile root whose ownership boundary changed.
        path: PathBuf,
        /// Stable profile-boundary failure description.
        reason: String,
    },
    /// The library database could not be initialized or became unusable.
    DatabaseUnavailable {
        /// Stable database failure description.
        reason: String,
    },
}

impl std::fmt::Display for GlobalLibraryWriterUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProfileOwnershipChanged { path, reason } => write!(
                formatter,
                "profile ownership changed at {}: {reason}",
                path.display()
            ),
            Self::DatabaseUnavailable { reason } => {
                write!(formatter, "global library database unavailable: {reason}")
            }
        }
    }
}

/// Nonblocking command-admission result for the global-library owner.
#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum GlobalLibraryWriterQueueError {
    /// The owner is still applying schema and migrations.
    #[error("global library writer is initializing")]
    Initializing,
    /// The fixed-capacity owner queue is full.
    #[error("global library writer queue is full")]
    Full,
    /// Initialization failed or the owner is otherwise unavailable.
    #[error("global library writer is unavailable: {0}")]
    Unavailable(GlobalLibraryWriterUnavailable),
    /// A bounded UI-preferences payload was rejected before queue admission.
    #[error("UI preferences payload exceeds the {limit}-byte limit ({actual} bytes)")]
    PayloadTooLarge {
        /// Maximum accepted payload size.
        limit: usize,
        /// Supplied payload size.
        actual: usize,
    },
    /// A profile vocabulary request was rejected before queue admission.
    #[error("invalid profile tag vocabulary input: {0}")]
    InvalidProfileTagVocabulary(#[from] ProfileTagVocabularyInputError),
    /// The owner is closing or has already closed.
    #[error("global library writer is closed")]
    Closed,
}

/// Failure to start a profile-owned global-library worker.
#[derive(Debug, Error)]
pub enum GlobalLibraryWriterStartError {
    /// The supplied profile capability was no longer valid before handoff.
    #[error(transparent)]
    ProfileOwnership(#[from] ProfileOwnershipError),
    /// The operating system rejected creation of the owner thread.
    #[error("failed to start global library writer thread: {0}")]
    ThreadSpawn(#[source] std::io::Error),
}

enum WriterCommand {
    LoadSourceRegistry {
        result: SyncSender<Result<SourceRegistrySnapshot, LibraryError>>,
    },
    LookupRetainedSource {
        root: PathBuf,
        result: SyncSender<Result<Option<SampleSource>, LibraryError>>,
    },
    ReplaceSourceRegistry {
        snapshot: SourceRegistrySnapshot,
        result: SyncSender<Result<(), LibraryError>>,
    },
    LoadUiPreferences {
        result: SyncSender<Result<Option<String>, LibraryError>>,
    },
    ReplaceUiPreferences {
        payload: String,
        result: SyncSender<Result<(), LibraryError>>,
    },
    LookupProfileTagCategories {
        labels: Vec<String>,
        result: SyncSender<Result<ProfileTagVocabularyLookup, LibraryError>>,
    },
    ProfileTagVocabularyPage {
        expected_revision: Option<u64>,
        prefix: String,
        after_cursor: Option<ProfileTagVocabularyPageCursor>,
        requested_limit: usize,
        result: SyncSender<Result<ProfileTagVocabularyPage, LibraryError>>,
    },
    SetProfileTagCategory {
        expected_revision: u64,
        label: String,
        category: ProfileTagCategory,
        result: SyncSender<Result<ProfileTagVocabularySetReceipt, LibraryError>>,
    },
}

enum WriterReply {
    Load {
        result: SyncSender<Result<SourceRegistrySnapshot, LibraryError>>,
        value: Result<SourceRegistrySnapshot, LibraryError>,
    },
    LookupRetainedSource {
        result: SyncSender<Result<Option<SampleSource>, LibraryError>>,
        value: Result<Option<SampleSource>, LibraryError>,
    },
    Replace {
        result: SyncSender<Result<(), LibraryError>>,
        value: Result<(), LibraryError>,
    },
    LoadUiPreferences {
        result: SyncSender<Result<Option<String>, LibraryError>>,
        value: Result<Option<String>, LibraryError>,
    },
    ReplaceUiPreferences {
        result: SyncSender<Result<(), LibraryError>>,
        value: Result<(), LibraryError>,
    },
    LookupProfileTagCategories {
        result: SyncSender<Result<ProfileTagVocabularyLookup, LibraryError>>,
        value: Result<ProfileTagVocabularyLookup, LibraryError>,
    },
    ProfileTagVocabularyPage {
        result: SyncSender<Result<ProfileTagVocabularyPage, LibraryError>>,
        value: Result<ProfileTagVocabularyPage, LibraryError>,
    },
    SetProfileTagCategory {
        result: SyncSender<Result<ProfileTagVocabularySetReceipt, LibraryError>>,
        value: Result<ProfileTagVocabularySetReceipt, LibraryError>,
    },
}

impl WriterReply {
    fn send(self) {
        match self {
            Self::Load { result, value } => {
                let _ = result.send(value);
            }
            Self::LookupRetainedSource { result, value } => {
                let _ = result.send(value);
            }
            Self::Replace { result, value } => {
                let _ = result.send(value);
            }
            Self::LoadUiPreferences { result, value } => {
                let _ = result.send(value);
            }
            Self::ReplaceUiPreferences { result, value } => {
                let _ = result.send(value);
            }
            Self::LookupProfileTagCategories { result, value } => {
                let _ = result.send(value);
            }
            Self::ProfileTagVocabularyPage { result, value } => {
                let _ = result.send(value);
            }
            Self::SetProfileTagCategory { result, value } => {
                let _ = result.send(value);
            }
        }
    }

    fn send_error(self, error: LibraryError) {
        match self {
            Self::Load { result, .. } => {
                let _ = result.send(Err(error));
            }
            Self::LookupRetainedSource { result, .. } => {
                let _ = result.send(Err(error));
            }
            Self::Replace { result, .. } => {
                let _ = result.send(Err(error));
            }
            Self::LoadUiPreferences { result, .. } => {
                let _ = result.send(Err(error));
            }
            Self::ReplaceUiPreferences { result, .. } => {
                let _ = result.send(Err(error));
            }
            Self::LookupProfileTagCategories { result, .. } => {
                let _ = result.send(Err(error));
            }
            Self::ProfileTagVocabularyPage { result, .. } => {
                let _ = result.send(Err(error));
            }
            Self::SetProfileTagCategory { result, .. } => {
                let _ = result.send(Err(error));
            }
        }
    }
}

struct OwnerHooks {
    initialization_gate: Option<Arc<std::sync::Barrier>>,
    #[cfg(test)]
    binding_open_gate: Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>,
    #[cfg(test)]
    post_command_gate: Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>,
}

/// Profile-owned asynchronous writer for `library.db`.
///
/// The handle contains only typed command and lifecycle state. The writable SQLite connection
/// remains on one owner thread for the handle's lifetime and is never exposed to callers.
pub struct GlobalLibraryWriter {
    commands: Arc<Mutex<Option<SyncSender<WriterCommand>>>>,
    admission_gate: Arc<Mutex<()>>,
    lifecycle: Arc<std::sync::atomic::AtomicU8>,
    unavailable: Arc<Mutex<Option<GlobalLibraryWriterUnavailable>>>,
    worker: Option<JoinHandle<()>>,
    #[cfg(test)]
    blocked_receiver: Option<Receiver<WriterCommand>>,
}

/// Cloneable access to the profile-owned tag vocabulary commands.
///
/// Clones share the writer's bounded admission gate and lifecycle. They do not own a database
/// connection or an owner thread; the [`GlobalLibraryWriter`] remains responsible for shutdown.
#[derive(Clone)]
pub struct ProfileTagVocabularyClient {
    commands: Arc<Mutex<Option<SyncSender<WriterCommand>>>>,
    admission_gate: Arc<Mutex<()>>,
    lifecycle: Arc<std::sync::atomic::AtomicU8>,
    unavailable: Arc<Mutex<Option<GlobalLibraryWriterUnavailable>>>,
}

impl GlobalLibraryWriter {
    /// Start an owner using a clone of an already-acquired profile capability.
    ///
    /// Cloning the capability duplicates its retained descriptors without acquiring a second
    /// profile lock. The supplied guard remains owned by its caller until this writer is shut
    /// down, which lets a profile owner coordinate release order explicitly.
    pub fn start(
        profile_guard: &WritableProfileGuard,
    ) -> Result<Self, GlobalLibraryWriterStartError> {
        Self::start_inner(profile_guard, None, None, None)
    }

    fn start_inner(
        profile_guard: &WritableProfileGuard,
        initialization_gate: Option<Arc<std::sync::Barrier>>,
        binding_open_gate: Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>,
        post_command_gate: Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>,
    ) -> Result<Self, GlobalLibraryWriterStartError> {
        let worker_profile_guard = profile_guard.try_clone()?;
        let (commands, command_rx) = mpsc::sync_channel(GLOBAL_LIBRARY_WRITER_COMMAND_CAPACITY);
        let commands = Arc::new(Mutex::new(Some(commands)));
        let lifecycle = Arc::new(std::sync::atomic::AtomicU8::new(
            WriterLifecycle::Initializing as u8,
        ));
        let worker_lifecycle = Arc::clone(&lifecycle);
        let unavailable = Arc::new(Mutex::new(None));
        let worker_unavailable = Arc::clone(&unavailable);
        let admission_gate = Arc::new(Mutex::new(()));
        let worker_admission_gate = Arc::clone(&admission_gate);
        #[cfg(not(test))]
        let _ = (binding_open_gate, post_command_gate);
        let worker = thread::Builder::new()
            .name(String::from("wavecrate-global-library-writer"))
            .spawn(move || {
                run_owner(
                    worker_profile_guard,
                    command_rx,
                    worker_lifecycle,
                    worker_unavailable,
                    OwnerHooks {
                        initialization_gate,
                        #[cfg(test)]
                        binding_open_gate,
                        #[cfg(test)]
                        post_command_gate,
                    },
                    worker_admission_gate,
                );
            })
            .map_err(GlobalLibraryWriterStartError::ThreadSpawn)?;

        Ok(Self {
            commands: Arc::clone(&commands),
            admission_gate,
            lifecycle,
            unavailable,
            worker: Some(worker),
            #[cfg(test)]
            blocked_receiver: None,
        })
    }

    #[cfg(test)]
    fn start_with_initialization_gate_for_test(
        profile_guard: &WritableProfileGuard,
    ) -> (Self, Arc<std::sync::Barrier>) {
        let gate = Arc::new(std::sync::Barrier::new(2));
        let writer = Self::start_inner(profile_guard, Some(Arc::clone(&gate)), None, None)
            .expect("writer start");
        (writer, gate)
    }

    #[cfg(test)]
    fn start_with_binding_open_gate_for_test(
        profile_guard: &WritableProfileGuard,
    ) -> (Self, Arc<std::sync::Barrier>, Arc<std::sync::Barrier>) {
        let ready = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let writer = Self::start_inner(
            profile_guard,
            None,
            Some((Arc::clone(&ready), Arc::clone(&release))),
            None,
        )
        .expect("writer start");
        (writer, ready, release)
    }

    #[cfg(test)]
    fn start_with_post_command_gate_for_test(
        profile_guard: &WritableProfileGuard,
    ) -> (Self, Arc<std::sync::Barrier>, Arc<std::sync::Barrier>) {
        let ready = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let writer = Self::start_inner(
            profile_guard,
            None,
            None,
            Some((Arc::clone(&ready), Arc::clone(&release))),
        )
        .expect("writer start");
        (writer, ready, release)
    }

    #[cfg(test)]
    fn disabled() -> Self {
        let (commands, blocked_receiver) =
            mpsc::sync_channel(GLOBAL_LIBRARY_WRITER_COMMAND_CAPACITY);
        Self {
            commands: Arc::new(Mutex::new(Some(commands))),
            admission_gate: Arc::new(Mutex::new(())),
            lifecycle: Arc::new(std::sync::atomic::AtomicU8::new(
                WriterLifecycle::Available as u8,
            )),
            unavailable: Arc::new(Mutex::new(None)),
            worker: None,
            blocked_receiver: Some(blocked_receiver),
        }
    }

    /// Return the current owner lifecycle without waiting for initialization or I/O.
    pub fn status(&self) -> GlobalLibraryWriterStatus {
        writer_status(&self.lifecycle, &self.unavailable)
    }

    /// Return the retained reason for an initialization or profile-boundary failure, if any.
    pub fn unavailable_reason(&self) -> Option<GlobalLibraryWriterUnavailable> {
        self.unavailable
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Return a cloneable client for the profile-owned tag vocabulary commands.
    pub fn profile_tag_vocabulary_client(&self) -> ProfileTagVocabularyClient {
        ProfileTagVocabularyClient {
            commands: Arc::clone(&self.commands),
            admission_gate: Arc::clone(&self.admission_gate),
            lifecycle: Arc::clone(&self.lifecycle),
            unavailable: Arc::clone(&self.unavailable),
        }
    }

    /// Queue a load of the immutable source-registry snapshot.
    pub fn load_source_registry_snapshot(
        &self,
    ) -> Result<Receiver<Result<SourceRegistrySnapshot, LibraryError>>, GlobalLibraryWriterQueueError>
    {
        self.ensure_available()?;
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        self.send(WriterCommand::LoadSourceRegistry { result: result_tx })?;
        Ok(result_rx)
    }

    /// Queue a lookup of the retained descriptor for one source root.
    ///
    /// The root is owned by the queued command so the lookup remains confined to the profile
    /// owner thread. This preserves historical source ids and metadata policy without exposing
    /// the library database connection to callers.
    pub fn lookup_retained_source_for_root(
        &self,
        root: PathBuf,
    ) -> Result<Receiver<Result<Option<SampleSource>, LibraryError>>, GlobalLibraryWriterQueueError>
    {
        self.ensure_available()?;
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        self.send(WriterCommand::LookupRetainedSource {
            root,
            result: result_tx,
        })?;
        Ok(result_rx)
    }

    /// Queue an idempotent replacement of the source-registry snapshot.
    pub fn replace_source_registry(
        &self,
        snapshot: SourceRegistrySnapshot,
    ) -> Result<Receiver<Result<(), LibraryError>>, GlobalLibraryWriterQueueError> {
        self.ensure_available()?;
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        self.send(WriterCommand::ReplaceSourceRegistry {
            snapshot,
            result: result_tx,
        })?;
        Ok(result_rx)
    }

    /// Queue a bounded load of the versioned GPUI preferences payload.
    pub fn load_ui_preferences_payload(
        &self,
    ) -> Result<Receiver<Result<Option<String>, LibraryError>>, GlobalLibraryWriterQueueError> {
        self.ensure_available()?;
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        self.send(WriterCommand::LoadUiPreferences { result: result_tx })?;
        Ok(result_rx)
    }

    /// Queue an atomic replacement of the bounded GPUI preferences payload.
    pub fn replace_ui_preferences_payload(
        &self,
        payload: String,
    ) -> Result<Receiver<Result<(), LibraryError>>, GlobalLibraryWriterQueueError> {
        if payload.len() > UI_PREFERENCES_MAX_BYTES {
            return Err(GlobalLibraryWriterQueueError::PayloadTooLarge {
                limit: UI_PREFERENCES_MAX_BYTES,
                actual: payload.len(),
            });
        }
        self.ensure_available()?;
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        self.send(WriterCommand::ReplaceUiPreferences {
            payload,
            result: result_tx,
        })?;
        Ok(result_rx)
    }

    /// Queue an exact lookup of bounded profile vocabulary labels.
    ///
    /// Labels are validated and exact duplicates are removed before admission. The result is
    /// bounded to 64 rows.
    pub fn lookup_profile_tag_categories(
        &self,
        labels: Vec<String>,
    ) -> Result<
        Receiver<Result<ProfileTagVocabularyLookup, LibraryError>>,
        GlobalLibraryWriterQueueError,
    > {
        self.profile_tag_vocabulary_client()
            .lookup_profile_tag_categories(labels)
    }

    /// Queue one bounded literal-prefix profile vocabulary page.
    ///
    /// The requested page size is clamped to `1..=64`. A continuation cursor is opaque and may
    /// only be reused with the exact revision and literal prefix that produced it.
    pub fn profile_tag_vocabulary_page(
        &self,
        expected_revision: Option<u64>,
        prefix: String,
        after_cursor: Option<ProfileTagVocabularyPageCursor>,
        requested_limit: usize,
    ) -> Result<
        Receiver<Result<ProfileTagVocabularyPage, LibraryError>>,
        GlobalLibraryWriterQueueError,
    > {
        self.profile_tag_vocabulary_client()
            .profile_tag_vocabulary_page(expected_revision, prefix, after_cursor, requested_limit)
    }

    /// Queue one category assignment at an exact profile vocabulary revision.
    pub fn set_profile_tag_category(
        &self,
        expected_revision: u64,
        label: String,
        category: ProfileTagCategory,
    ) -> Result<
        Receiver<Result<ProfileTagVocabularySetReceipt, LibraryError>>,
        GlobalLibraryWriterQueueError,
    > {
        self.profile_tag_vocabulary_client()
            .set_profile_tag_category(expected_revision, label, category)
    }

    /// Stop new admission, drain accepted commands, close `library.db`, and join the owner.
    pub fn shutdown(&mut self) -> bool {
        let previous = self.lifecycle.swap(
            WriterLifecycle::Closing as u8,
            std::sync::atomic::Ordering::AcqRel,
        );
        let sender = {
            let _admission = self
                .admission_gate
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.commands
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
        };
        drop(sender);
        #[cfg(test)]
        self.blocked_receiver.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.lifecycle.store(
            WriterLifecycle::Closed as u8,
            std::sync::atomic::Ordering::Release,
        );
        WriterLifecycle::from_u8(previous) != WriterLifecycle::Closed
    }

    fn ensure_available(&self) -> Result<(), GlobalLibraryWriterQueueError> {
        ensure_available_state(&self.lifecycle, &self.unavailable)
    }

    fn send(&self, command: WriterCommand) -> Result<(), GlobalLibraryWriterQueueError> {
        send_command(
            &self.commands,
            &self.admission_gate,
            &self.lifecycle,
            &self.unavailable,
            command,
        )
    }
}

impl ProfileTagVocabularyClient {
    /// Return the current shared writer lifecycle without waiting for I/O.
    pub fn status(&self) -> GlobalLibraryWriterStatus {
        writer_status(&self.lifecycle, &self.unavailable)
    }

    /// Queue an exact lookup of bounded profile vocabulary labels.
    pub fn lookup_profile_tag_categories(
        &self,
        mut labels: Vec<String>,
    ) -> Result<
        Receiver<Result<ProfileTagVocabularyLookup, LibraryError>>,
        GlobalLibraryWriterQueueError,
    > {
        validate_lookup_labels(&mut labels)?;
        self.ensure_available()?;
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        self.send(WriterCommand::LookupProfileTagCategories {
            labels,
            result: result_tx,
        })?;
        Ok(result_rx)
    }

    /// Queue one bounded literal-prefix profile vocabulary page.
    pub fn profile_tag_vocabulary_page(
        &self,
        expected_revision: Option<u64>,
        prefix: String,
        after_cursor: Option<ProfileTagVocabularyPageCursor>,
        requested_limit: usize,
    ) -> Result<
        Receiver<Result<ProfileTagVocabularyPage, LibraryError>>,
        GlobalLibraryWriterQueueError,
    > {
        validate_page_inputs(&prefix, after_cursor.as_ref())?;
        self.ensure_available()?;
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        self.send(WriterCommand::ProfileTagVocabularyPage {
            expected_revision,
            prefix,
            after_cursor,
            requested_limit,
            result: result_tx,
        })?;
        Ok(result_rx)
    }

    /// Queue one category assignment at an exact profile vocabulary revision.
    pub fn set_profile_tag_category(
        &self,
        expected_revision: u64,
        label: String,
        category: ProfileTagCategory,
    ) -> Result<
        Receiver<Result<ProfileTagVocabularySetReceipt, LibraryError>>,
        GlobalLibraryWriterQueueError,
    > {
        validate_set_input(&label, category)?;
        self.ensure_available()?;
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        self.send(WriterCommand::SetProfileTagCategory {
            expected_revision,
            label,
            category,
            result: result_tx,
        })?;
        Ok(result_rx)
    }

    fn ensure_available(&self) -> Result<(), GlobalLibraryWriterQueueError> {
        ensure_available_state(&self.lifecycle, &self.unavailable)
    }

    fn send(&self, command: WriterCommand) -> Result<(), GlobalLibraryWriterQueueError> {
        send_command(
            &self.commands,
            &self.admission_gate,
            &self.lifecycle,
            &self.unavailable,
            command,
        )
    }
}

fn writer_status(
    lifecycle: &Arc<std::sync::atomic::AtomicU8>,
    unavailable: &Arc<Mutex<Option<GlobalLibraryWriterUnavailable>>>,
) -> GlobalLibraryWriterStatus {
    match WriterLifecycle::from_u8(lifecycle.load(std::sync::atomic::Ordering::Acquire)) {
        WriterLifecycle::Initializing => GlobalLibraryWriterStatus::Initializing,
        WriterLifecycle::Available => GlobalLibraryWriterStatus::Available,
        WriterLifecycle::Unavailable => GlobalLibraryWriterStatus::Unavailable {
            reason: unavailable
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
                .unwrap_or_else(|| GlobalLibraryWriterUnavailable::DatabaseUnavailable {
                    reason: String::from("global library writer is unavailable"),
                }),
        },
        WriterLifecycle::Closing => GlobalLibraryWriterStatus::Closing,
        WriterLifecycle::Closed => GlobalLibraryWriterStatus::Closed,
    }
}

fn ensure_available_state(
    lifecycle: &Arc<std::sync::atomic::AtomicU8>,
    unavailable: &Arc<Mutex<Option<GlobalLibraryWriterUnavailable>>>,
) -> Result<(), GlobalLibraryWriterQueueError> {
    match WriterLifecycle::from_u8(lifecycle.load(std::sync::atomic::Ordering::Acquire)) {
        WriterLifecycle::Initializing => Err(GlobalLibraryWriterQueueError::Initializing),
        WriterLifecycle::Available => Ok(()),
        WriterLifecycle::Unavailable => Err(GlobalLibraryWriterQueueError::Unavailable(
            unavailable
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
                .unwrap_or_else(|| GlobalLibraryWriterUnavailable::DatabaseUnavailable {
                    reason: String::from("global library writer is unavailable"),
                }),
        )),
        WriterLifecycle::Closing | WriterLifecycle::Closed => {
            Err(GlobalLibraryWriterQueueError::Closed)
        }
    }
}

fn send_command(
    commands: &Arc<Mutex<Option<SyncSender<WriterCommand>>>>,
    admission_gate: &Arc<Mutex<()>>,
    lifecycle: &Arc<std::sync::atomic::AtomicU8>,
    unavailable: &Arc<Mutex<Option<GlobalLibraryWriterUnavailable>>>,
    command: WriterCommand,
) -> Result<(), GlobalLibraryWriterQueueError> {
    let _admission = admission_gate
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    ensure_available_state(lifecycle, unavailable)?;
    let sender = commands
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .cloned()
        .ok_or(GlobalLibraryWriterQueueError::Closed)?;
    sender.try_send(command).map_err(|error| match error {
        TrySendError::Full(_) => GlobalLibraryWriterQueueError::Full,
        TrySendError::Disconnected(_) => GlobalLibraryWriterQueueError::Closed,
    })
}

impl Drop for GlobalLibraryWriter {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WriterLifecycle {
    Initializing = 0,
    Available = 1,
    Unavailable = 2,
    Closing = 3,
    Closed = 4,
}

impl WriterLifecycle {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Available,
            2 => Self::Unavailable,
            3 => Self::Closing,
            4 => Self::Closed,
            _ => Self::Initializing,
        }
    }
}

fn run_owner(
    profile_guard: WritableProfileGuard,
    command_rx: Receiver<WriterCommand>,
    lifecycle: Arc<std::sync::atomic::AtomicU8>,
    unavailable: Arc<Mutex<Option<GlobalLibraryWriterUnavailable>>>,
    hooks: OwnerHooks,
    admission_gate: Arc<Mutex<()>>,
) {
    if let Some(initialization_gate) = hooks.initialization_gate {
        initialization_gate.wait();
    }
    #[cfg(test)]
    let open_result =
        LibraryDatabase::open_for_profile_guard(&profile_guard, hooks.binding_open_gate);
    #[cfg(not(test))]
    let open_result = { LibraryDatabase::open_for_profile_guard(&profile_guard) };
    let mut database = match open_result {
        Ok(database) => database,
        Err(error) => {
            let profile_error = profile_guard.validate_current().err();
            let reason = profile_error
                .as_ref()
                .map(unavailable_from_profile_error)
                .unwrap_or_else(|| unavailable_from_library_error(&error));
            *unavailable
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(reason);
            lifecycle.store(
                if profile_error.is_some() || matches!(error, LibraryError::ProfileOwnership(_)) {
                    WriterLifecycle::Closed as u8
                } else {
                    WriterLifecycle::Unavailable as u8
                },
                std::sync::atomic::Ordering::Release,
            );
            return;
        }
    };

    if let Err(error) = database.validate_profile_guard(&profile_guard) {
        close_after_profile_change(
            &command_rx,
            &lifecycle,
            &unavailable,
            &admission_gate,
            error,
        );
        return;
    }
    lifecycle.store(
        WriterLifecycle::Available as u8,
        std::sync::atomic::Ordering::Release,
    );

    loop {
        if let Err(error) = database.validate_profile_guard(&profile_guard) {
            close_after_profile_change(
                &command_rx,
                &lifecycle,
                &unavailable,
                &admission_gate,
                error,
            );
            break;
        }
        let command = match command_rx.recv_timeout(OWNER_POLL_INTERVAL) {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if let Err(error) = database.validate_profile_guard(&profile_guard) {
            reject_after_profile_change(command, &error);
            close_after_profile_change(
                &command_rx,
                &lifecycle,
                &unavailable,
                &admission_gate,
                error,
            );
            break;
        }

        let reply = execute_command(command, &mut database);

        #[cfg(test)]
        if let Some((ready, release)) = hooks.post_command_gate.as_ref() {
            ready.wait();
            release.wait();
        }

        if let Err(error) = database.validate_profile_guard(&profile_guard) {
            reply.send_error(profile_change_library_error(&error, true));
            close_after_profile_change(
                &command_rx,
                &lifecycle,
                &unavailable,
                &admission_gate,
                error,
            );
            break;
        }
        reply.send();
    }

    drop(database);
    lifecycle.store(
        WriterLifecycle::Closed as u8,
        std::sync::atomic::Ordering::Release,
    );
}

fn execute_command(command: WriterCommand, database: &mut LibraryDatabase) -> WriterReply {
    match command {
        WriterCommand::LoadSourceRegistry { result } => WriterReply::Load {
            value: database.load_source_registry(),
            result,
        },
        WriterCommand::LookupRetainedSource { root, result } => WriterReply::LookupRetainedSource {
            value: database.lookup_retained_source(&root),
            result,
        },
        WriterCommand::ReplaceSourceRegistry { snapshot, result } => WriterReply::Replace {
            value: database.replace_source_registry(&snapshot),
            result,
        },
        WriterCommand::LoadUiPreferences { result } => WriterReply::LoadUiPreferences {
            value: database.load_ui_preferences_payload(),
            result,
        },
        WriterCommand::ReplaceUiPreferences { payload, result } => {
            WriterReply::ReplaceUiPreferences {
                value: database.replace_ui_preferences_payload(&payload),
                result,
            }
        }
        WriterCommand::LookupProfileTagCategories { labels, result } => {
            WriterReply::LookupProfileTagCategories {
                value: database.lookup_profile_tag_categories(&labels),
                result,
            }
        }
        WriterCommand::ProfileTagVocabularyPage {
            expected_revision,
            prefix,
            after_cursor,
            requested_limit,
            result,
        } => WriterReply::ProfileTagVocabularyPage {
            value: database.profile_tag_vocabulary_page(
                expected_revision,
                &prefix,
                after_cursor.as_ref(),
                requested_limit,
            ),
            result,
        },
        WriterCommand::SetProfileTagCategory {
            expected_revision,
            label,
            category,
            result,
        } => WriterReply::SetProfileTagCategory {
            value: database.set_profile_tag_category(expected_revision, &label, category),
            result,
        },
    }
}

fn close_after_profile_change(
    command_rx: &Receiver<WriterCommand>,
    lifecycle: &Arc<std::sync::atomic::AtomicU8>,
    unavailable: &Arc<Mutex<Option<GlobalLibraryWriterUnavailable>>>,
    admission_gate: &Arc<Mutex<()>>,
    error: ProfileOwnershipError,
) {
    let _admission = admission_gate
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let reason = unavailable_from_profile_error(&error);
    *unavailable
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(reason.clone());
    lifecycle.store(
        WriterLifecycle::Closed as u8,
        std::sync::atomic::Ordering::Release,
    );
    reject_remaining_profile_change(command_rx, &error);
}

fn reject_after_profile_change(command: WriterCommand, error: &ProfileOwnershipError) {
    send_command_error(command, profile_change_library_error(error, false));
}

fn reject_remaining_profile_change(
    command_rx: &Receiver<WriterCommand>,
    error: &ProfileOwnershipError,
) {
    while let Ok(command) = command_rx.try_recv() {
        reject_after_profile_change(command, error);
    }
}

fn send_command_error(command: WriterCommand, error: LibraryError) {
    match command {
        WriterCommand::LoadSourceRegistry { result } => {
            let _ = result.send(Err(error));
        }
        WriterCommand::LookupRetainedSource { result, .. } => {
            let _ = result.send(Err(error));
        }
        WriterCommand::ReplaceSourceRegistry { result, .. } => {
            let _ = result.send(Err(error));
        }
        WriterCommand::LoadUiPreferences { result } => {
            let _ = result.send(Err(error));
        }
        WriterCommand::ReplaceUiPreferences { result, .. } => {
            let _ = result.send(Err(error));
        }
        WriterCommand::LookupProfileTagCategories { result, .. } => {
            let _ = result.send(Err(error));
        }
        WriterCommand::ProfileTagVocabularyPage { result, .. } => {
            let _ = result.send(Err(error));
        }
        WriterCommand::SetProfileTagCategory { result, .. } => {
            let _ = result.send(Err(error));
        }
    }
}

fn profile_change_library_error(
    error: &ProfileOwnershipError,
    completion_not_confirmable: bool,
) -> LibraryError {
    let reason = if completion_not_confirmable {
        format!("completion not confirmable: {error}")
    } else {
        error.to_string()
    };
    LibraryError::ProfileOwnershipChanged {
        path: profile_error_path(error),
        reason,
    }
}

fn profile_error_path(error: &ProfileOwnershipError) -> PathBuf {
    match error {
        ProfileOwnershipError::ProfileRootReplaced { path }
        | ProfileOwnershipError::ProfileOwnerLockReplaced { path }
        | ProfileOwnershipError::ProfileOwnedByAnotherProcess { path }
        | ProfileOwnershipError::Io { path, .. }
        | ProfileOwnershipError::NotRegularFile { path }
        | ProfileOwnershipError::IdentityUnavailable { path }
        | ProfileOwnershipError::ProfileChildAlreadyLeased { path }
        | ProfileOwnershipError::Unsupported { path } => path.clone(),
        ProfileOwnershipError::AppDirectory(_) => PathBuf::new(),
    }
}

fn unavailable_from_profile_error(error: &ProfileOwnershipError) -> GlobalLibraryWriterUnavailable {
    GlobalLibraryWriterUnavailable::ProfileOwnershipChanged {
        path: profile_error_path(error),
        reason: error.to_string(),
    }
}

fn unavailable_from_library_error(error: &LibraryError) -> GlobalLibraryWriterUnavailable {
    match error {
        LibraryError::ProfileOwnership(error) => unavailable_from_profile_error(error),
        LibraryError::ProfileOwnershipChanged { path, reason } => {
            GlobalLibraryWriterUnavailable::ProfileOwnershipChanged {
                path: path.clone(),
                reason: reason.clone(),
            }
        }
        error => GlobalLibraryWriterUnavailable::DatabaseUnavailable {
            reason: error.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard};
    use crate::sample_sources::{SourceId, SourceRole};
    use crate::test_runtime::TestRuntimeGuard;
    use rusqlite::{Connection, params};
    use std::fs;
    use std::path::Path;
    use std::thread;
    use std::time::{Duration, Instant};
    use tempfile::tempdir;

    fn wait_until_available(writer: &GlobalLibraryWriter) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match writer.status() {
                GlobalLibraryWriterStatus::Available => return,
                GlobalLibraryWriterStatus::Unavailable { reason } => {
                    panic!("writer unavailable: {reason}")
                }
                status if Instant::now() >= deadline => panic!("writer status: {status:?}"),
                _ => thread::sleep(Duration::from_millis(5)),
            }
        }
    }

    fn wait_until_closed(writer: &GlobalLibraryWriter) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !matches!(writer.status(), GlobalLibraryWriterStatus::Closed) {
            assert!(Instant::now() < deadline, "writer did not close");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn source(root: &Path, id: &str) -> SampleSource {
        SampleSource {
            id: SourceId::from_string(id),
            root: root.to_path_buf(),
            role: SourceRole::Normal,
            metadata_storage: crate::sample_sources::SourceMetadataStorage::SourceFolder,
            primary_import_folder: crate::sample_sources::default_primary_import_folder(),
        }
    }

    fn start_writer(
        base: &tempfile::TempDir,
        name: &str,
    ) -> (
        ConfigBaseGuard,
        PersistenceProfileGuard,
        WritableProfileGuard,
        GlobalLibraryWriter,
    ) {
        let base_path = std::fs::canonicalize(base.path()).expect("canonical test base");
        let base_guard = ConfigBaseGuard::set(base_path);
        let profile_guard = PersistenceProfileGuard::named(name);
        let writable_guard = WritableProfileGuard::acquire_current().expect("profile ownership");
        let writer = GlobalLibraryWriter::start(&writable_guard).expect("writer start");
        (base_guard, profile_guard, writable_guard, writer)
    }

    #[test]
    fn queue_admission_distinguishes_initializing_and_full() {
        let temp = tempdir().expect("base");
        let _runtime = TestRuntimeGuard::acquire();
        let base_guard =
            ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).expect("canonical test base"));
        let profile_guard = PersistenceProfileGuard::named("queue");
        let writable_guard = WritableProfileGuard::acquire_current().expect("profile ownership");
        let (writer, gate) =
            GlobalLibraryWriter::start_with_initialization_gate_for_test(&writable_guard);
        assert_eq!(writer.status(), GlobalLibraryWriterStatus::Initializing);
        assert!(matches!(
            writer.load_source_registry_snapshot(),
            Err(GlobalLibraryWriterQueueError::Initializing)
        ));
        gate.wait();
        wait_until_available(&writer);
        drop(writer);
        drop(writable_guard);
        drop(profile_guard);
        drop(base_guard);
    }

    #[test]
    fn queue_admission_is_nonblocking_and_reports_full() {
        let writer = GlobalLibraryWriter::disabled();
        for _ in 0..GLOBAL_LIBRARY_WRITER_COMMAND_CAPACITY {
            writer
                .load_source_registry_snapshot()
                .expect("bounded queue admission");
        }
        assert!(matches!(
            writer.load_source_registry_snapshot(),
            Err(GlobalLibraryWriterQueueError::Full)
        ));
    }

    #[test]
    fn source_registry_snapshot_owns_an_immutable_copy() {
        let temp = tempdir().expect("base");
        let mut original = source(&temp.path().join("original"), "source");
        let snapshot = SourceRegistrySnapshot::new(vec![original.clone()]);
        original.root = temp.path().join("mutated");

        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot.as_slice()[0].root, temp.path().join("original"));
    }

    #[test]
    fn writer_keeps_shared_profile_ownership_after_original_guard_is_dropped() {
        let temp = tempdir().expect("base");
        let _runtime = TestRuntimeGuard::acquire();
        let base_path = std::fs::canonicalize(temp.path()).expect("canonical test base");
        let base_guard = ConfigBaseGuard::set(base_path);
        let profile_guard = PersistenceProfileGuard::named("shared-ownership");
        let writable_guard = WritableProfileGuard::acquire_current().expect("profile ownership");
        let mut writer = GlobalLibraryWriter::start(&writable_guard).expect("writer start");
        wait_until_available(&writer);
        drop(writable_guard);

        assert!(matches!(
            WritableProfileGuard::acquire_current(),
            Err(crate::app_dirs::ProfileOwnershipError::ProfileOwnedByAnotherProcess { .. })
        ));
        assert!(writer.shutdown());
        let replacement = WritableProfileGuard::acquire_current().expect("released ownership");
        drop(replacement);
        drop(profile_guard);
        drop(base_guard);
    }

    #[test]
    fn one_owner_connection_serves_repeated_commands_and_shutdown_drains() {
        let temp = tempdir().expect("base");
        let _runtime = TestRuntimeGuard::acquire();
        let (base_guard, profile_guard, writable_guard, mut writer) =
            start_writer(&temp, "one-connection");
        let _ = (&base_guard, &profile_guard);
        wait_until_available(&writer);
        let root = temp.path().join("source");
        let snapshot = SourceRegistrySnapshot::new(vec![source(&root, "source-1")]);
        for _ in 0..3 {
            writer
                .replace_source_registry(snapshot.clone())
                .expect("replace admission")
                .recv_timeout(Duration::from_secs(2))
                .expect("replace result")
                .expect("replace success");
        }
        let loaded = writer
            .load_source_registry_snapshot()
            .expect("load admission")
            .recv_timeout(Duration::from_secs(2))
            .expect("load result")
            .expect("load success");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.as_slice()[0].id.as_str(), "source-1");
        assert_eq!(
            super::super::connection::test_open_count(
                &writable_guard
                    .profile_root()
                    .join(super::super::LIBRARY_DB_FILE_NAME)
            ),
            1
        );
        assert!(writer.shutdown());
        assert!(matches!(writer.status(), GlobalLibraryWriterStatus::Closed));
    }

    #[test]
    fn ui_preferences_payload_is_bounded_and_reopens_through_same_owner() {
        let temp = tempdir().expect("base");
        let _runtime = TestRuntimeGuard::acquire();
        let (base_guard, profile_guard, _writable_guard, mut writer) =
            start_writer(&temp, "ui-preferences");
        let _ = (&base_guard, &profile_guard);
        wait_until_available(&writer);

        let oversized = "x".repeat(super::super::UI_PREFERENCES_MAX_BYTES + 1);
        assert!(matches!(
            writer.replace_ui_preferences_payload(oversized),
            Err(GlobalLibraryWriterQueueError::PayloadTooLarge { .. })
        ));
        assert!(matches!(
            writer.status(),
            GlobalLibraryWriterStatus::Available
        ));

        let payload = String::from("{\"schema_version\":1,\"theme\":{}}");
        writer
            .replace_ui_preferences_payload(payload.clone())
            .expect("payload admission")
            .recv_timeout(Duration::from_secs(2))
            .expect("payload result")
            .expect("payload replacement");
        let loaded = writer
            .load_ui_preferences_payload()
            .expect("payload load admission")
            .recv_timeout(Duration::from_secs(2))
            .expect("payload load result")
            .expect("payload load");
        assert_eq!(loaded.as_deref(), Some(payload.as_str()));
        assert!(writer.shutdown());
    }

    #[test]
    fn profile_vocabulary_owner_validates_before_admission_and_drains_on_shutdown() {
        let temp = tempdir().expect("base");
        let _runtime = TestRuntimeGuard::acquire();
        let (base_guard, profile_guard, _writable_guard, mut writer) =
            start_writer(&temp, "profile-vocabulary-owner");
        let _ = (&base_guard, &profile_guard);
        wait_until_available(&writer);

        let lookup = writer
            .lookup_profile_tag_categories(vec![String::from("kick"), String::from("kick")])
            .expect("lookup admission")
            .recv_timeout(Duration::from_secs(2))
            .expect("lookup result")
            .expect("lookup success");
        assert_eq!(lookup.revision, 0);
        assert!(lookup.entries.is_empty());

        let inserted = writer
            .set_profile_tag_category(0, String::from("kick"), ProfileTagCategory::SoundType)
            .expect("set admission")
            .recv_timeout(Duration::from_secs(2))
            .expect("set result")
            .expect("set success");
        assert_eq!(inserted.revision, 1);
        assert!(inserted.changed);

        let oversized = "x".repeat(super::super::PROFILE_TAG_VOCABULARY_MAX_LABEL_BYTES + 1);
        assert!(matches!(
            writer.set_profile_tag_category(1, oversized, ProfileTagCategory::Character),
            Err(GlobalLibraryWriterQueueError::InvalidProfileTagVocabulary(
                ProfileTagVocabularyInputError::LabelTooLong { .. }
            ))
        ));
        assert!(matches!(
            writer.status(),
            GlobalLibraryWriterStatus::Available
        ));

        assert!(writer.shutdown());
        assert!(matches!(
            writer.lookup_profile_tag_categories(vec![String::from("kick")]),
            Err(GlobalLibraryWriterQueueError::Closed)
        ));
    }

    #[test]
    fn retained_profile_vocabulary_client_follows_owner_shutdown() {
        let temp = tempdir().expect("base");
        let _runtime = TestRuntimeGuard::acquire();
        let (base_guard, profile_guard, _writable_guard, mut writer) =
            start_writer(&temp, "profile-vocabulary-client");
        let _ = (&base_guard, &profile_guard);
        wait_until_available(&writer);
        let client = writer.profile_tag_vocabulary_client();
        assert!(matches!(
            client.status(),
            GlobalLibraryWriterStatus::Available
        ));
        let lookup = client
            .lookup_profile_tag_categories(vec![String::from("kick")])
            .expect("client lookup admission")
            .recv_timeout(Duration::from_secs(2))
            .expect("client lookup result")
            .expect("client lookup success");
        assert!(lookup.entries.is_empty());

        assert!(writer.shutdown());
        assert!(matches!(client.status(), GlobalLibraryWriterStatus::Closed));
        assert!(matches!(
            client.lookup_profile_tag_categories(vec![String::from("kick")]),
            Err(GlobalLibraryWriterQueueError::Closed)
        ));
    }

    #[test]
    fn ui_preferences_replacement_is_atomic_on_metadata_failure() {
        let temp = tempdir().expect("base");
        let _runtime = TestRuntimeGuard::acquire();
        let (base_guard, profile_guard, _writable_guard, mut writer) =
            start_writer(&temp, "ui-preferences-rollback");
        let _ = (&base_guard, &profile_guard);
        wait_until_available(&writer);

        let original = String::from("original");
        writer
            .replace_ui_preferences_payload(original.clone())
            .expect("initial payload admission")
            .recv_timeout(Duration::from_secs(2))
            .expect("initial payload result")
            .expect("initial payload replacement");

        let connection = crate::sample_sources::library::open_connection().expect("open fixture");
        connection
            .execute_batch(
                "CREATE TRIGGER fail_ui_preferences_update
                 BEFORE UPDATE OF value ON metadata
                 WHEN OLD.key = 'ui_preferences_v1'
                 BEGIN SELECT RAISE(FAIL, 'UI preferences rollback fixture'); END;",
            )
            .expect("rollback trigger");
        drop(connection);

        let result = writer
            .replace_ui_preferences_payload(String::from("replacement"))
            .expect("replacement admission")
            .recv_timeout(Duration::from_secs(2))
            .expect("replacement result");
        assert!(matches!(result, Err(LibraryError::Sql(_))));
        let loaded = writer
            .load_ui_preferences_payload()
            .expect("load admission")
            .recv_timeout(Duration::from_secs(2))
            .expect("load result")
            .expect("load payload");
        assert_eq!(loaded.as_deref(), Some(original.as_str()));
        assert!(writer.shutdown());
    }

    #[test]
    fn invalid_utf8_preferences_are_reported_without_replacing_the_row() {
        let temp = tempdir().expect("base");
        let _runtime = TestRuntimeGuard::acquire();
        let base_path = std::fs::canonicalize(temp.path()).expect("canonical test base");
        let base_guard = ConfigBaseGuard::set(base_path);
        let profile_guard = PersistenceProfileGuard::named("ui-preferences-invalid");
        let connection = crate::sample_sources::library::open_connection().expect("fixture DB");
        connection
            .execute(
                "INSERT INTO metadata (key, value) VALUES (?1, CAST(?2 AS BLOB))
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![super::super::UI_PREFERENCES_KEY, vec![0xff_u8]],
            )
            .expect("invalid payload fixture");
        drop(connection);
        let writable_guard = WritableProfileGuard::acquire_current().expect("profile ownership");
        let mut writer = GlobalLibraryWriter::start(&writable_guard).expect("writer start");
        wait_until_available(&writer);
        let result = writer
            .load_ui_preferences_payload()
            .expect("payload load admission")
            .recv_timeout(Duration::from_secs(2))
            .expect("payload load result");
        assert!(matches!(
            result,
            Err(LibraryError::MetadataInvalidUtf8 { .. })
        ));
        assert!(matches!(
            writer.status(),
            GlobalLibraryWriterStatus::Available
        ));
        assert!(writer.shutdown());
        drop(writable_guard);
        drop(profile_guard);
        drop(base_guard);
    }

    #[test]
    fn failed_replacement_rolls_back_active_sources() {
        let temp = tempdir().expect("base");
        let _runtime = TestRuntimeGuard::acquire();
        let base_path = std::fs::canonicalize(temp.path()).expect("canonical test base");
        let base_guard = ConfigBaseGuard::set(base_path);
        let profile_guard = PersistenceProfileGuard::named("rollback");
        let original = source(&temp.path().join("original"), "original");
        crate::sample_sources::library::save(&crate::sample_sources::library::LibraryState {
            sources: vec![original.clone()],
        })
        .expect("initial source");
        let connection = crate::sample_sources::library::open_connection().expect("fixture DB");
        connection
            .execute_batch(
                "CREATE TRIGGER fail_known_source_update
                 BEFORE UPDATE OF value ON metadata
                 WHEN OLD.key = 'known_sources_v1'
                 BEGIN SELECT RAISE(FAIL, 'rollback fixture'); END;",
            )
            .expect("rollback trigger");
        drop(connection);
        let writable_guard = WritableProfileGuard::acquire_current().expect("profile ownership");
        let mut writer = GlobalLibraryWriter::start(&writable_guard).expect("writer start");
        wait_until_available(&writer);
        let replacement = SourceRegistrySnapshot::new(vec![source(
            &temp.path().join("replacement"),
            "replacement",
        )]);
        assert!(matches!(
            writer
                .replace_source_registry(replacement)
                .expect("replace admission")
                .recv_timeout(Duration::from_secs(2))
                .expect("replace result"),
            Err(LibraryError::Sql(_))
        ));
        let loaded = writer
            .load_source_registry_snapshot()
            .expect("load admission")
            .recv_timeout(Duration::from_secs(2))
            .expect("load result")
            .expect("load success");
        assert_eq!(loaded.as_slice()[0].id.as_str(), original.id.as_str());
        assert!(writer.shutdown());
        drop(writable_guard);
        drop(profile_guard);
        drop(base_guard);
    }

    #[test]
    fn source_registry_and_known_sources_survive_writer_reopen() {
        let temp = tempdir().expect("base");
        let _runtime = TestRuntimeGuard::acquire();
        let base_path = std::fs::canonicalize(temp.path()).expect("canonical test base");
        let base_guard = ConfigBaseGuard::set(base_path);
        let profile_guard = PersistenceProfileGuard::named("reopen");
        let root = temp.path().join("persistent-source");
        let retained = source(&root, "persistent-id");
        let writable_guard = WritableProfileGuard::acquire_current().expect("profile ownership");
        let mut writer = GlobalLibraryWriter::start(&writable_guard).expect("writer start");
        wait_until_available(&writer);
        writer
            .replace_source_registry(SourceRegistrySnapshot::new(vec![retained.clone()]))
            .expect("initial replace admission")
            .recv_timeout(Duration::from_secs(2))
            .expect("initial replace result")
            .expect("initial replace");
        assert!(writer.shutdown());
        drop(writable_guard);

        let writable_guard = WritableProfileGuard::acquire_current().expect("reopen ownership");
        let mut writer = GlobalLibraryWriter::start(&writable_guard).expect("reopen writer");
        wait_until_available(&writer);
        let loaded = writer
            .load_source_registry_snapshot()
            .expect("reopen load admission")
            .recv_timeout(Duration::from_secs(2))
            .expect("reopen load result")
            .expect("reopen load");
        assert_eq!(loaded.as_slice()[0].id.as_str(), retained.id.as_str());
        writer
            .replace_source_registry(SourceRegistrySnapshot::new(Vec::new()))
            .expect("remove admission")
            .recv_timeout(Duration::from_secs(2))
            .expect("remove result")
            .expect("remove");
        let retained_lookup = writer
            .lookup_retained_source_for_root(root.clone())
            .expect("retained lookup admission")
            .recv_timeout(Duration::from_secs(2))
            .expect("retained lookup result")
            .expect("retained lookup");
        assert_eq!(
            retained_lookup.expect("retained descriptor").id,
            retained.id
        );
        assert!(writer.shutdown());
        drop(writable_guard);

        assert_eq!(
            crate::sample_sources::library::lookup_retained_source_for_root(&root)
                .expect("retained source lookup")
                .expect("retained source")
                .id
                .as_str(),
            retained.id.as_str()
        );
        drop(profile_guard);
        drop(base_guard);
    }

    #[test]
    fn corrupt_database_is_unavailable_without_repeated_open_attempts() {
        let temp = tempdir().expect("base");
        let _runtime = TestRuntimeGuard::acquire();
        let base_guard =
            ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).expect("canonical test base"));
        let profile_guard = PersistenceProfileGuard::named("corrupt");
        let writable_guard = WritableProfileGuard::acquire_current().expect("profile ownership");
        let path = writable_guard
            .profile_root()
            .join(super::super::LIBRARY_DB_FILE_NAME);
        fs::write(&path, b"not sqlite").expect("corrupt fixture");
        let writer = GlobalLibraryWriter::start(&writable_guard).expect("writer start");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !matches!(
            writer.status(),
            GlobalLibraryWriterStatus::Unavailable { .. }
        ) {
            assert!(
                Instant::now() < deadline,
                "writer did not become unavailable"
            );
            thread::sleep(Duration::from_millis(5));
        }
        assert!(matches!(
            writer.load_source_registry_snapshot(),
            Err(GlobalLibraryWriterQueueError::Unavailable(
                GlobalLibraryWriterUnavailable::DatabaseUnavailable { .. }
            ))
        ));
        assert_eq!(super::super::connection::test_open_count(&path), 1);
        drop(writer);
        drop(writable_guard);
        drop(profile_guard);
        drop(base_guard);
    }

    #[test]
    fn profile_owned_open_rejects_nonregular_sidecar_before_sqlite() {
        let temp = tempdir().expect("base");
        let _runtime = TestRuntimeGuard::acquire();
        let base_path = std::fs::canonicalize(temp.path()).expect("canonical test base");
        let base_guard = ConfigBaseGuard::set(base_path);
        let profile_guard = PersistenceProfileGuard::named("sidecar-entry");
        let writable_guard = WritableProfileGuard::acquire_current().expect("profile ownership");
        let root = writable_guard.profile_root().to_path_buf();
        fs::create_dir(root.join("library.db-wal")).expect("nonregular WAL fixture");
        let db_path = root.join(super::super::LIBRARY_DB_FILE_NAME);
        let opens_before = super::super::connection::test_open_count(&db_path);
        let writer = GlobalLibraryWriter::start(&writable_guard).expect("writer start");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !matches!(
            writer.status(),
            GlobalLibraryWriterStatus::Unavailable { .. }
        ) {
            assert!(
                Instant::now() < deadline,
                "writer did not reject the nonregular sidecar"
            );
            thread::sleep(Duration::from_millis(5));
        }
        assert!(matches!(
            writer.status(),
            GlobalLibraryWriterStatus::Unavailable {
                reason: GlobalLibraryWriterUnavailable::DatabaseUnavailable { reason }
            } if reason.contains("library.db-wal")
        ));
        assert_eq!(
            super::super::connection::test_open_count(&db_path),
            opens_before
        );
        drop(writer);
        drop(writable_guard);
        drop(profile_guard);
        drop(base_guard);
    }

    #[cfg(unix)]
    #[test]
    fn profile_owned_open_rejects_database_symlink_without_following_it() {
        let temp = tempdir().expect("base");
        let _runtime = TestRuntimeGuard::acquire();
        let base_path = std::fs::canonicalize(temp.path()).expect("canonical test base");
        let base_guard = ConfigBaseGuard::set(base_path);
        let profile_guard = PersistenceProfileGuard::named("database-symlink");
        let writable_guard = WritableProfileGuard::acquire_current().expect("profile ownership");
        let root = writable_guard.profile_root().to_path_buf();
        let outside = temp.path().join("outside.db");
        fs::write(&outside, b"outside database").expect("outside database");
        std::os::unix::fs::symlink(&outside, root.join(super::super::LIBRARY_DB_FILE_NAME))
            .expect("database symlink fixture");
        let db_path = root.join(super::super::LIBRARY_DB_FILE_NAME);
        let opens_before = super::super::connection::test_open_count(&db_path);
        let writer = GlobalLibraryWriter::start(&writable_guard).expect("writer start");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !matches!(
            writer.status(),
            GlobalLibraryWriterStatus::Unavailable { .. }
        ) {
            assert!(
                Instant::now() < deadline,
                "writer did not reject the database symlink"
            );
            thread::sleep(Duration::from_millis(5));
        }
        assert!(matches!(
            writer.status(),
            GlobalLibraryWriterStatus::Unavailable {
                reason: GlobalLibraryWriterUnavailable::DatabaseUnavailable { reason }
            } if reason.contains("library.db")
        ));
        assert_eq!(
            super::super::connection::test_open_count(&db_path),
            opens_before
        );
        drop(writer);
        drop(writable_guard);
        drop(profile_guard);
        drop(base_guard);
    }

    #[cfg(unix)]
    #[test]
    fn profile_root_replacement_during_initialization_does_not_redirect_database() {
        let temp = tempdir().expect("base");
        let _runtime = TestRuntimeGuard::acquire();
        let base_path = std::fs::canonicalize(temp.path()).expect("canonical test base");
        let base_guard = ConfigBaseGuard::set(base_path);
        let profile_guard = PersistenceProfileGuard::named("initialization-replacement");
        let writable_guard = WritableProfileGuard::acquire_current().expect("profile ownership");
        let root = writable_guard.profile_root().to_path_buf();
        let (mut writer, ready, release) =
            GlobalLibraryWriter::start_with_binding_open_gate_for_test(&writable_guard);

        ready.wait();
        let displaced = temp.path().join("initialization-replacement-old");
        fs::rename(&root, &displaced).expect("displace original profile root");
        fs::create_dir(&root).expect("replacement profile root");
        release.wait();

        wait_until_closed(&writer);
        assert!(!root.join(super::super::LIBRARY_DB_FILE_NAME).exists());
        assert!(displaced.join(super::super::LIBRARY_DB_FILE_NAME).is_file());
        assert!(matches!(
            writer.unavailable_reason(),
            Some(GlobalLibraryWriterUnavailable::ProfileOwnershipChanged { path, .. })
                if path == root
        ));
        assert!(!writer.shutdown());
        drop(writable_guard);
        drop(profile_guard);
        drop(base_guard);
    }

    #[cfg(unix)]
    #[test]
    fn profile_replacement_before_reply_returns_completion_not_confirmable() {
        let temp = tempdir().expect("base");
        let _runtime = TestRuntimeGuard::acquire();
        let base_path = std::fs::canonicalize(temp.path()).expect("canonical test base");
        let base_guard = ConfigBaseGuard::set(base_path);
        let profile_guard = PersistenceProfileGuard::named("inflight-replacement");
        let writable_guard = WritableProfileGuard::acquire_current().expect("profile ownership");
        let (mut writer, ready, release) =
            GlobalLibraryWriter::start_with_post_command_gate_for_test(&writable_guard);
        wait_until_available(&writer);
        let root = writable_guard.profile_root().to_path_buf();
        let result = writer
            .replace_source_registry(SourceRegistrySnapshot::new(Vec::new()))
            .expect("replace admission");

        ready.wait();
        let queued_result = writer
            .load_source_registry_snapshot()
            .expect("queued load admission");
        let displaced = temp.path().join("inflight-replacement-old");
        fs::rename(&root, &displaced).expect("displace original profile root");
        fs::create_dir(&root).expect("replacement profile root");
        release.wait();

        let result = result
            .recv_timeout(Duration::from_secs(2))
            .expect("in-flight command result");
        match result {
            Err(LibraryError::ProfileOwnershipChanged { path, reason }) => {
                assert_eq!(path, root);
                assert!(reason.contains("completion not confirmable"));
            }
            other => panic!("profile replacement returned {other:?}"),
        }
        assert!(matches!(
            queued_result
                .recv_timeout(Duration::from_secs(2))
                .expect("queued command result"),
            Err(LibraryError::ProfileOwnershipChanged { .. })
        ));
        wait_until_closed(&writer);
        assert!(matches!(
            writer.load_source_registry_snapshot(),
            Err(GlobalLibraryWriterQueueError::Closed)
        ));
        assert!(displaced.join(super::super::LIBRARY_DB_FILE_NAME).is_file());
        assert!(!writer.shutdown());
        drop(writable_guard);
        drop(profile_guard);
        drop(base_guard);
    }

    #[test]
    #[cfg(unix)]
    fn profile_replacement_closes_owner_and_rejects_later_commands() {
        let temp = tempdir().expect("base");
        let _runtime = TestRuntimeGuard::acquire();
        let (base_guard, profile_guard, writable_guard, mut writer) =
            start_writer(&temp, "replacement");
        let root = writable_guard.profile_root().to_path_buf();
        wait_until_available(&writer);
        let displaced = temp.path().join("displaced-profile");
        fs::rename(&root, &displaced).expect("displace profile");
        fs::create_dir(&root).expect("replacement profile");
        wait_until_closed(&writer);
        assert!(matches!(
            writer.load_source_registry_snapshot(),
            Err(GlobalLibraryWriterQueueError::Closed)
        ));
        assert!(matches!(
            writer.unavailable_reason(),
            Some(GlobalLibraryWriterUnavailable::ProfileOwnershipChanged { .. })
        ));
        let _ = writer.shutdown();
        drop(writable_guard);
        drop(profile_guard);
        drop(base_guard);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn profile_lock_replacement_closes_owner_and_rejects_later_commands() {
        let temp = tempdir().expect("base");
        let _runtime = TestRuntimeGuard::acquire();
        let (base_guard, profile_guard, writable_guard, mut writer) =
            start_writer(&temp, "lock-replacement");
        let root = writable_guard.profile_root().to_path_buf();
        wait_until_available(&writer);
        let lock_path = root.join("profile-owner.lock");
        let displaced = root.join("profile-owner.lock.old");
        fs::rename(&lock_path, &displaced).expect("displace profile lock");
        fs::write(&lock_path, b"replacement").expect("replacement profile lock");
        wait_until_closed(&writer);
        assert!(matches!(
            writer.load_source_registry_snapshot(),
            Err(GlobalLibraryWriterQueueError::Closed)
        ));
        assert!(matches!(
            writer.unavailable_reason(),
            Some(GlobalLibraryWriterUnavailable::ProfileOwnershipChanged { path, .. })
                if path == lock_path
        ));
        let _ = writer.shutdown();
        drop(writable_guard);
        drop(profile_guard);
        drop(base_guard);
    }

    #[cfg(windows)]
    #[test]
    fn profile_root_rename_is_blocked_by_retained_database_binding() {
        let temp = tempdir().expect("base");
        let _runtime = TestRuntimeGuard::acquire();
        let (base_guard, profile_guard, writable_guard, mut writer) =
            start_writer(&temp, "root-rename-barrier");
        let root = writable_guard.profile_root().to_path_buf();
        wait_until_available(&writer);
        let displaced = temp.path().join("root-rename-barrier-old");
        assert!(fs::rename(&root, &displaced).is_err());
        assert!(matches!(
            writer.status(),
            GlobalLibraryWriterStatus::Available
        ));
        assert!(writer.shutdown());
        drop(writable_guard);
        drop(profile_guard);
        drop(base_guard);
    }

    #[test]
    fn shutdown_closes_database_before_guard_can_be_released() {
        let temp = tempdir().expect("base");
        let _runtime = TestRuntimeGuard::acquire();
        let (base_guard, profile_guard, writable_guard, mut writer) =
            start_writer(&temp, "shutdown");
        wait_until_available(&writer);
        let snapshot = SourceRegistrySnapshot::new(vec![source(&temp.path().join("source"), "id")]);
        let result = writer
            .replace_source_registry(snapshot)
            .expect("replace admission");
        assert!(writer.shutdown());
        assert!(
            result
                .recv_timeout(Duration::from_secs(2))
                .expect("drained command")
                .is_ok()
        );
        assert!(matches!(writer.status(), GlobalLibraryWriterStatus::Closed));
        drop(writable_guard);
        drop(profile_guard);
        drop(base_guard);
        let _ = Connection::open(
            temp.path()
                .join(".wavecrate")
                .join("profiles")
                .join("shutdown")
                .join("library.db"),
        )
        .expect("closed database is reopenable");
    }
}
