//! Profile-owned asynchronous ownership for the global library database.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use thiserror::Error;

use super::connection::LibraryDatabase;
use super::error::LibraryError;
use super::harvest_authored::{
    HarvestAuthoredRequest, HarvestAuthoredResult, HarvestAuthoredSnapshot,
    HarvestAuthoredSnapshotRequest,
};
use super::harvest_copy::{HarvestCopyRequest, HarvestCopyResult};
use super::harvest_facts::{self, HarvestFactsBatch, HarvestFactsRequest};
use super::mutation_context::{
    GlobalMutationCompletion, GlobalMutationKind, GlobalMutationRequest, ProfileAcquisitionId,
    SnapshotFences,
};
use super::operation_link_participant::{OperationLinkRequest, OperationLinkResult};
use super::reference_participant::{
    ReferenceRekeyRequest, ReferenceRekeyResult, ReferenceSnapshotRequest,
};
use super::reference_snapshot::ReferenceSnapshot;
use super::registry_limits::{
    RegistryBoundError, SOURCE_REGISTRY_MAX_BYTES, SOURCE_REGISTRY_MAX_DESCRIPTOR_BYTES,
    SOURCE_REGISTRY_MAX_DESCRIPTORS, SOURCE_REGISTRY_MAX_SOURCE_ID_BYTES, path_bytes,
    validate_sources_encoded,
};
use super::registry_participant::{
    SourceRegistryParticipantReceipt, SourceRegistryParticipantRequest,
};
use super::tag_vocabulary::{
    ProfileTagCategory, ProfileTagVocabularyDeleteReceipt, ProfileTagVocabularyInputError,
    ProfileTagVocabularyLookup, ProfileTagVocabularyPage, ProfileTagVocabularyPageCursor,
    ProfileTagVocabularySetReceipt, validate_delete_input, validate_lookup_labels,
    validate_page_inputs, validate_set_input,
};
use crate::app_dirs::{ProfileChildLease, ProfileOwnershipError, WritableProfileGuard};
use crate::sample_sources::{SampleSource, UI_PREFERENCES_MAX_BYTES};

mod dispatch_metrics;
pub use dispatch_metrics::GlobalDispatchMetricsSnapshot;
use dispatch_metrics::QueuedCommand;

mod admission_metrics;
pub use admission_metrics::GlobalAdmissionMetricsSnapshot;

mod shutdown;
pub use shutdown::{GlobalLibraryWriterDrainOutcome, GlobalLibraryWriterDrainReport};

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

    pub(super) fn validate_for_admission(&self) -> Result<(), GlobalLibraryWriterQueueError> {
        validate_sources_encoded(&self.sources)
            .map(|_| ())
            .map_err(GlobalLibraryWriterQueueError::from_registry_bound_error)
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
    /// The worker unwound; accepted effects and replies may be unconfirmed.
    WorkerPanicked,
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
            Self::WorkerPanicked => formatter.write_str("global library owner thread panicked"),
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
    /// A vocabulary-only client cannot submit a different metadata route.
    #[error("non-vocabulary request submitted through vocabulary client")]
    InvalidMutationRoute,
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
    /// A source-registry request exceeded its descriptor count bound.
    #[error("source registry contains too many descriptors ({actual}, max {limit})")]
    SourceRegistryTooManyDescriptors {
        /// Maximum descriptor count.
        limit: usize,
        /// Supplied descriptor count.
        actual: usize,
    },
    /// A source-registry descriptor or path exceeded its bounded size.
    #[error("source registry descriptor {index} field {field} exceeds {limit} bytes ({actual})")]
    SourceRegistryFieldTooLarge {
        /// Descriptor index.
        index: usize,
        /// Field whose bound was exceeded.
        field: &'static str,
        /// Maximum accepted field size.
        limit: usize,
        /// Supplied field size.
        actual: usize,
    },
    /// A source-registry source identity exceeded the journal identity bound.
    #[error("source registry descriptor {index} source identity exceeds {limit} bytes ({actual})")]
    SourceRegistrySourceIdTooLarge {
        /// Descriptor index.
        index: usize,
        /// Maximum accepted identity size.
        limit: usize,
        /// Supplied identity size.
        actual: usize,
    },
    /// A source-registry descriptor or encoded payload exceeded the aggregate bound.
    #[error("source registry payload exceeds {limit} bytes ({actual})")]
    SourceRegistryPayloadTooLarge {
        /// Maximum accepted encoded size.
        limit: usize,
        /// Supplied or observed encoded size.
        actual: usize,
    },
    /// A source-registry path could not fit the descriptor field bound.
    #[error("source registry path exceeds {limit} bytes ({actual})")]
    SourceRegistryPathTooLarge {
        /// Maximum accepted path size.
        limit: usize,
        /// Supplied path size.
        actual: usize,
    },
    /// A profile vocabulary request was rejected before queue admission.
    #[error("invalid profile tag vocabulary input: {0}")]
    InvalidProfileTagVocabulary(#[from] ProfileTagVocabularyInputError),
    /// The owner is closing or has already closed.
    #[error("global library writer is closed")]
    Closed,
}

impl GlobalLibraryWriterQueueError {
    pub(super) fn from_registry_bound_error(error: RegistryBoundError) -> Self {
        match error {
            RegistryBoundError::TooManyDescriptors { actual } => {
                Self::SourceRegistryTooManyDescriptors {
                    limit: SOURCE_REGISTRY_MAX_DESCRIPTORS,
                    actual,
                }
            }
            RegistryBoundError::AggregateTooLarge { actual } => {
                Self::SourceRegistryPayloadTooLarge {
                    limit: SOURCE_REGISTRY_MAX_BYTES,
                    actual,
                }
            }
            RegistryBoundError::DescriptorTooLarge { index, actual } => {
                Self::SourceRegistryFieldTooLarge {
                    index,
                    field: "descriptor",
                    limit: SOURCE_REGISTRY_MAX_DESCRIPTOR_BYTES,
                    actual,
                }
            }
            RegistryBoundError::FieldTooLarge {
                index,
                field,
                actual,
            } => Self::SourceRegistryFieldTooLarge {
                index,
                field,
                limit: SOURCE_REGISTRY_MAX_DESCRIPTOR_BYTES,
                actual,
            },
            RegistryBoundError::SourceIdTooLarge { index, actual } => {
                Self::SourceRegistrySourceIdTooLarge {
                    index,
                    limit: SOURCE_REGISTRY_MAX_SOURCE_ID_BYTES,
                    actual,
                }
            }
            RegistryBoundError::ArithmeticOverflow | RegistryBoundError::Serialization => {
                Self::SourceRegistryPayloadTooLarge {
                    limit: SOURCE_REGISTRY_MAX_BYTES,
                    actual: SOURCE_REGISTRY_MAX_BYTES.saturating_add(1),
                }
            }
        }
    }
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
    HarvestFacts {
        request: HarvestFactsRequest,
        result: SyncSender<Result<HarvestFactsBatch, LibraryError>>,
    },
    HarvestAuthored {
        request: HarvestAuthoredRequest,
        result: SyncSender<Result<HarvestAuthoredResult, LibraryError>>,
    },
    HarvestAuthoredSnapshot {
        request: HarvestAuthoredSnapshotRequest,
        result: SyncSender<Result<HarvestAuthoredSnapshot, LibraryError>>,
    },

    HarvestCopy {
        request: HarvestCopyRequest,
        result: SyncSender<Result<HarvestCopyResult, LibraryError>>,
    },
    #[cfg(any(test, feature = "test-support"))]
    PanicForTest {
        result: SyncSender<Result<(), LibraryError>>,
    },
    ReferenceSnapshot {
        request: ReferenceSnapshotRequest,
        result: SyncSender<Result<ReferenceSnapshot, LibraryError>>,
    },
    ReferenceRekey {
        request: ReferenceRekeyRequest,
        result: SyncSender<Result<ReferenceRekeyResult, LibraryError>>,
    },
    CapturedMutation {
        request: GlobalMutationRequest,
        result: SyncSender<GlobalMutationCompletion>,
    },
    ApplyOperationLink {
        request: OperationLinkRequest,
        result: SyncSender<Result<OperationLinkResult, LibraryError>>,
    },
    ApplySourceRegistryParticipant {
        request: SourceRegistryParticipantRequest,
        result: SyncSender<Result<SourceRegistryParticipantReceipt, LibraryError>>,
    },
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
    DeleteProfileTagVocabulary {
        expected_revision: u64,
        label: String,
        result: SyncSender<Result<ProfileTagVocabularyDeleteReceipt, LibraryError>>,
    },
}

enum WriterReply {
    HarvestFacts {
        request: HarvestFactsRequest,
        value: Result<HarvestFactsBatch, LibraryError>,
        result: SyncSender<Result<HarvestFactsBatch, LibraryError>>,
    },
    HarvestAuthored {
        request: HarvestAuthoredRequest,
        result: SyncSender<Result<HarvestAuthoredResult, LibraryError>>,
        value: Result<HarvestAuthoredResult, LibraryError>,
    },
    HarvestAuthoredSnapshot {
        request: HarvestAuthoredSnapshotRequest,
        result: SyncSender<Result<HarvestAuthoredSnapshot, LibraryError>>,
        value: Result<HarvestAuthoredSnapshot, LibraryError>,
    },

    HarvestCopy {
        request: HarvestCopyRequest,
        value: Result<HarvestCopyResult, LibraryError>,
        result: SyncSender<Result<HarvestCopyResult, LibraryError>>,
    },
    ReferenceSnapshot {
        request: ReferenceSnapshotRequest,
        value: Result<ReferenceSnapshot, LibraryError>,
        result: SyncSender<Result<ReferenceSnapshot, LibraryError>>,
    },
    ReferenceRekey {
        request: ReferenceRekeyRequest,
        value: Result<ReferenceRekeyResult, LibraryError>,
        result: SyncSender<Result<ReferenceRekeyResult, LibraryError>>,
    },
    CapturedMutation {
        value: GlobalMutationCompletion,
        result: SyncSender<GlobalMutationCompletion>,
    },
    OperationLink {
        result: SyncSender<Result<OperationLinkResult, LibraryError>>,
        value: Result<OperationLinkResult, LibraryError>,
    },
    SourceRegistryParticipant {
        result: SyncSender<Result<SourceRegistryParticipantReceipt, LibraryError>>,
        value: Result<SourceRegistryParticipantReceipt, LibraryError>,
    },
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
    DeleteProfileTagVocabulary {
        result: SyncSender<Result<ProfileTagVocabularyDeleteReceipt, LibraryError>>,
        value: Result<ProfileTagVocabularyDeleteReceipt, LibraryError>,
    },
}

impl WriterReply {
    fn send(self) {
        match self {
            Self::HarvestFacts {
                request,
                result,
                value,
            } => {
                let value = if value.is_ok() {
                    request.validate_sources().and(value)
                } else {
                    value
                };
                let _ = result.send(value);
            }
            Self::HarvestAuthored {
                request,
                result,
                value,
            } => {
                let value = if value.is_ok() && request.validate_sources(false).is_err() {
                    Err(LibraryError::HarvestAuthoredCommitUncertain)
                } else {
                    value
                };
                let _ = result.send(value);
            }
            Self::HarvestAuthoredSnapshot {
                request,
                result,
                value,
            } => {
                let value = if value.is_ok() {
                    request.validate_sources().and(value)
                } else {
                    value
                };
                let _ = result.send(value);
            }
            Self::HarvestCopy {
                request,
                result,
                value,
            } => {
                let value = if value.is_ok() && request.validate_live(false).is_err() {
                    Err(LibraryError::HarvestCopyCommitUncertain)
                } else {
                    value
                };
                let _ = result.send(value);
            }
            Self::CapturedMutation { result, value } => {
                let _ = result.send(value);
            }
            Self::ReferenceSnapshot {
                result,
                value,
                request,
            } => {
                let value = if value.is_ok() {
                    request.validate_sources().and(value)
                } else {
                    value
                };
                let _ = result.send(value);
            }
            Self::ReferenceRekey {
                result,
                value,
                request,
            } => {
                let value = if value.is_ok() && request.validate_sources(false).is_err() {
                    Err(LibraryError::ReferenceCommitUncertain)
                } else {
                    value
                };
                let _ = result.send(value);
            }
            Self::OperationLink { result, value } => {
                let _ = result.send(value);
            }
            Self::SourceRegistryParticipant { result, value } => {
                let _ = result.send(value);
            }
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
            Self::DeleteProfileTagVocabulary { result, value } => {
                let _ = result.send(value);
            }
        }
    }

    fn send_error(self, error: LibraryError) {
        match self {
            Self::HarvestFacts { result, .. } => {
                let _ = result.send(Err(error));
            }
            Self::HarvestAuthored { result, .. } => {
                let _ = result.send(Err(error));
            }
            Self::HarvestAuthoredSnapshot { result, .. } => {
                let _ = result.send(Err(error));
            }
            Self::HarvestCopy { result, .. } => {
                let _ = result.send(Err(error));
            }
            Self::CapturedMutation { result, value } => {
                let _ = result.send(value.replace_result_with_error(error));
            }
            Self::ReferenceSnapshot { result, .. } => {
                let _ = result.send(Err(error));
            }
            Self::ReferenceRekey { result, .. } => {
                let _ = result.send(Err(error));
            }
            Self::OperationLink { result, .. } => {
                let _ = result.send(Err(error));
            }
            Self::SourceRegistryParticipant { result, .. } => {
                let _ = result.send(Err(error));
            }
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
            Self::DeleteProfileTagVocabulary { result, .. } => {
                let _ = result.send(Err(error));
            }
        }
    }
}

#[cfg(test)]
type DatabaseHook = Arc<dyn Fn(&rusqlite::Connection) + Send + Sync>;

struct OwnerHooks {
    initialization_gate: Option<Arc<std::sync::Barrier>>,
    #[cfg(test)]
    binding_open_gate: Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>,
    #[cfg(test)]
    post_command_gate: Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>,
    #[cfg(test)]
    database_hook: Option<DatabaseHook>,
}

/// Profile-owned asynchronous writer for `library.db`.
///
/// The handle contains only typed command and lifecycle state. The writable SQLite connection
/// remains on one owner thread for the handle's lifetime and is never exposed to callers.
pub struct GlobalLibraryWriter {
    acquisition: ProfileAcquisitionId,
    commands: Arc<Mutex<Option<SyncSender<QueuedCommand>>>>,
    admission_gate: Arc<Mutex<()>>,
    admission_metrics: Arc<admission_metrics::AdmissionMetrics>,
    lifecycle: Arc<std::sync::atomic::AtomicU8>,
    unavailable: Arc<Mutex<Option<GlobalLibraryWriterUnavailable>>>,
    worker: Option<JoinHandle<()>>,
    drain_report: Option<GlobalLibraryWriterDrainReport>,
    #[cfg(test)]
    blocked_receiver: Option<Receiver<QueuedCommand>>,
}

/// Cloneable access to the profile-owned tag vocabulary commands.
///
/// Clones share the writer's bounded admission gate and lifecycle. They do not own a database
/// connection or an owner thread; the [`GlobalLibraryWriter`] remains responsible for shutdown.
#[derive(Clone)]
pub struct ProfileTagVocabularyClient {
    acquisition: ProfileAcquisitionId,
    commands: Arc<Mutex<Option<SyncSender<QueuedCommand>>>>,
    admission_gate: Arc<Mutex<()>>,
    admission_metrics: Arc<admission_metrics::AdmissionMetrics>,
    lifecycle: Arc<std::sync::atomic::AtomicU8>,
    unavailable: Arc<Mutex<Option<GlobalLibraryWriterUnavailable>>>,
}

impl GlobalLibraryWriter {
    /// Observe this handle's acquisition before forming a delayed proposal. Not authority.
    pub fn acquisition_id(&self) -> ProfileAcquisitionId {
        self.acquisition.clone()
    }

    /// Submit an already captured immutable proposal without substituting this handle's ID.
    /// The existing worker validates its retained capability before/after execution.
    pub fn submit_captured_mutation(
        &self,
        request: GlobalMutationRequest,
    ) -> Result<Receiver<GlobalMutationCompletion>, GlobalLibraryWriterQueueError> {
        self.ensure_available()?;
        let (result, receiver) = mpsc::sync_channel(1);
        self.send(WriterCommand::CapturedMutation { request, result })?;
        Ok(receiver)
    }

    /// Start an owner using a clone of an already-acquired profile capability.
    ///
    /// Cloning the capability duplicates its retained descriptors without acquiring a second
    /// profile lock. The supplied guard remains owned by its caller until this writer is shut
    /// down, which lets a profile owner coordinate release order explicitly.
    pub fn start(
        profile_guard: &WritableProfileGuard,
    ) -> Result<Self, GlobalLibraryWriterStartError> {
        Self::start_inner(
            profile_guard,
            None,
            None,
            None,
            #[cfg(test)]
            None,
        )
    }

    fn start_inner(
        profile_guard: &WritableProfileGuard,
        initialization_gate: Option<Arc<std::sync::Barrier>>,
        binding_open_gate: Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>,
        post_command_gate: Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>,
        #[cfg(test)] database_hook: Option<DatabaseHook>,
    ) -> Result<Self, GlobalLibraryWriterStartError> {
        let global_writer_lease = profile_guard.acquire_named_lease("library.db")?;
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
            .name(String::from("yield-global-library-writer"))
            .spawn(move || {
                let _exit_status = shutdown::OwnerExitStatus::new(
                    Arc::clone(&worker_lifecycle),
                    Arc::clone(&worker_unavailable),
                );
                run_owner(
                    worker_profile_guard,
                    global_writer_lease,
                    command_rx,
                    worker_lifecycle,
                    worker_unavailable,
                    OwnerHooks {
                        initialization_gate,
                        #[cfg(test)]
                        binding_open_gate,
                        #[cfg(test)]
                        post_command_gate,
                        #[cfg(test)]
                        database_hook,
                    },
                    worker_admission_gate,
                );
            })
            .map_err(GlobalLibraryWriterStartError::ThreadSpawn)?;

        Ok(Self {
            acquisition: ProfileAcquisitionId::from_guard(profile_guard),
            commands: Arc::clone(&commands),
            admission_gate,
            admission_metrics: Arc::default(),
            lifecycle,
            unavailable,
            worker: Some(worker),
            drain_report: None,
            #[cfg(test)]
            blocked_receiver: None,
        })
    }

    #[cfg(test)]
    fn start_with_initialization_gate_for_test(
        profile_guard: &WritableProfileGuard,
    ) -> (Self, Arc<std::sync::Barrier>) {
        let gate = Arc::new(std::sync::Barrier::new(2));
        let writer = Self::start_inner(profile_guard, Some(Arc::clone(&gate)), None, None, None)
            .expect("writer start");
        (writer, gate)
    }

    #[cfg(all(test, unix))]
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
            None,
        )
        .expect("writer start");
        (writer, ready, release)
    }

    #[cfg(test)]
    pub(super) fn start_with_post_command_gate_for_test(
        profile_guard: &WritableProfileGuard,
    ) -> (Self, Arc<std::sync::Barrier>, Arc<std::sync::Barrier>) {
        let ready = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let writer = Self::start_inner(
            profile_guard,
            None,
            None,
            Some((Arc::clone(&ready), Arc::clone(&release))),
            None,
        )
        .expect("writer start");
        (writer, ready, release)
    }

    #[cfg(test)]
    pub(super) fn start_with_database_hook_for_test(
        profile_guard: &WritableProfileGuard,
        hook: DatabaseHook,
        post_command_gate: Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>,
    ) -> Result<Self, GlobalLibraryWriterStartError> {
        Self::start_inner(profile_guard, None, None, post_command_gate, Some(hook))
    }

    #[cfg(test)]
    fn disabled() -> Self {
        let (commands, blocked_receiver) =
            mpsc::sync_channel(GLOBAL_LIBRARY_WRITER_COMMAND_CAPACITY);
        Self {
            acquisition: ProfileAcquisitionId::test_only(),
            commands: Arc::new(Mutex::new(Some(commands))),
            admission_gate: Arc::new(Mutex::new(())),
            admission_metrics: Arc::default(),
            lifecycle: Arc::new(std::sync::atomic::AtomicU8::new(
                WriterLifecycle::Available as u8,
            )),
            unavailable: Arc::new(Mutex::new(None)),
            worker: None,
            drain_report: None,
            blocked_receiver: Some(blocked_receiver),
        }
    }

    /// Inject a worker unwind through the real queue for isolated teardown fixtures.
    #[cfg(any(test, feature = "test-support"))]
    pub fn panic_owner_for_test(
        &self,
    ) -> Result<Receiver<Result<(), LibraryError>>, GlobalLibraryWriterQueueError> {
        let (result, receiver) = mpsc::sync_channel(1);
        self.send(WriterCommand::PanicForTest { result })?;
        Ok(receiver)
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

    /// Observe this acquisition's actual worker dequeue counts and fixed wait buckets.
    /// Never queue depth, SQL execution, durable completion or capacity authority.
    pub fn dispatch_metrics(&self) -> GlobalDispatchMetricsSnapshot {
        self.admission_metrics.dispatch.snapshot()
    }

    /// Return a cloneable client for the profile-owned tag vocabulary commands.
    pub fn profile_tag_vocabulary_client(&self) -> ProfileTagVocabularyClient {
        ProfileTagVocabularyClient {
            acquisition: self.acquisition.clone(),
            commands: Arc::clone(&self.commands),
            admission_gate: Arc::clone(&self.admission_gate),
            admission_metrics: Arc::clone(&self.admission_metrics),
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
        let root_bytes = path_bytes(&root);
        if root_bytes > SOURCE_REGISTRY_MAX_DESCRIPTOR_BYTES {
            return Err(GlobalLibraryWriterQueueError::SourceRegistryPathTooLarge {
                limit: SOURCE_REGISTRY_MAX_DESCRIPTOR_BYTES,
                actual: root_bytes,
            });
        }
        self.ensure_available()?;
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        self.send(WriterCommand::LookupRetainedSource {
            root,
            result: result_tx,
        })?;
        Ok(result_rx)
    }

    /// Clone a read-only facade over the same retained command transport and closing state.
    pub fn harvest_facts_client(&self) -> harvest_facts::HarvestFactsClient {
        harvest_facts::HarvestFactsClient::from_queue(self.profile_tag_vocabulary_client())
    }
    /// Read a bounded coherent source-fenced Harvest snapshot on the retained worker.
    pub fn read_harvest_facts(
        &self,
        request: HarvestFactsRequest,
    ) -> Result<Receiver<Result<HarvestFactsBatch, LibraryError>>, GlobalLibraryWriterQueueError>
    {
        let (result, receiver) = mpsc::sync_channel(1);
        self.send(WriterCommand::HarvestFacts { request, result })?;
        Ok(receiver)
    }
    /// Dispatch one bounded source-fenced authored-state operation on the retained owner.
    pub fn set_harvest_authored_state(
        &self,
        request: HarvestAuthoredRequest,
    ) -> Result<Receiver<Result<HarvestAuthoredResult, LibraryError>>, GlobalLibraryWriterQueueError>
    {
        let (result, receiver) = mpsc::sync_channel(1);
        self.send(WriterCommand::HarvestAuthored { request, result })?;
        Ok(receiver)
    }
    /// Dispatch one bounded source-fenced authored-state operation on the retained owner.
    pub fn harvest_authored_snapshot(
        &self,
        request: HarvestAuthoredSnapshotRequest,
    ) -> Result<
        Receiver<Result<HarvestAuthoredSnapshot, LibraryError>>,
        GlobalLibraryWriterQueueError,
    > {
        let (result, receiver) = mpsc::sync_channel(1);
        self.send(WriterCommand::HarvestAuthoredSnapshot { request, result })?;
        Ok(receiver)
    }
    /// Commit one copy metadata outcome using genuine source/journal/global observations.
    pub fn apply_harvest_copy(
        &self,
        request: HarvestCopyRequest,
    ) -> Result<Receiver<Result<HarvestCopyResult, LibraryError>>, GlobalLibraryWriterQueueError>
    {
        self.ensure_available()?;
        let (result, receiver) = mpsc::sync_channel(1);
        self.send(WriterCommand::HarvestCopy { request, result })?;
        Ok(receiver)
    }
    /// Read one complete bounded Harvest reference snapshot through the actual global owner.
    pub fn reference_snapshot(
        &self,
        request: ReferenceSnapshotRequest,
    ) -> Result<Receiver<Result<ReferenceSnapshot, LibraryError>>, GlobalLibraryWriterQueueError>
    {
        self.ensure_available()?;
        let (result, receiver) = mpsc::sync_channel(1);
        self.send(WriterCommand::ReferenceSnapshot { request, result })?;
        Ok(receiver)
    }
    /// Persist one rekey disposition per exact operation/work key. Never file completion authority.
    pub fn apply_reference_rekey(
        &self,
        request: ReferenceRekeyRequest,
    ) -> Result<Receiver<Result<ReferenceRekeyResult, LibraryError>>, GlobalLibraryWriterQueueError>
    {
        self.ensure_available()?;
        let (result, receiver) = mpsc::sync_channel(1);
        self.send(WriterCommand::ReferenceRekey { request, result })?;
        Ok(receiver)
    }
    /// Queue one bounded durable source/global correlation through this existing owner.
    /// The receipt is an observation, never journal or filesystem completion authority.
    pub fn apply_operation_link(
        &self,
        request: OperationLinkRequest,
    ) -> Result<Receiver<Result<OperationLinkResult, LibraryError>>, GlobalLibraryWriterQueueError>
    {
        self.ensure_available()?;
        let (result, receiver) = mpsc::sync_channel(1);
        self.send(WriterCommand::ApplyOperationLink { request, result })?;
        Ok(receiver)
    }

    /// Queue one durable operation-scoped registry result through this existing owner.
    /// The request's acquisition identity is checked against the live guard before any effect;
    /// queue admission and the returned receipt remain observations, not journal authority.
    pub fn apply_source_registry_participant(
        &self,
        request: SourceRegistryParticipantRequest,
    ) -> Result<
        Receiver<Result<SourceRegistryParticipantReceipt, LibraryError>>,
        GlobalLibraryWriterQueueError,
    > {
        self.ensure_available()?;
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        self.send(WriterCommand::ApplySourceRegistryParticipant {
            request,
            result: result_tx,
        })?;
        Ok(result_rx)
    }

    /// Queue an idempotent replacement of the source-registry snapshot.
    pub fn replace_source_registry(
        &self,
        snapshot: SourceRegistrySnapshot,
    ) -> Result<Receiver<Result<(), LibraryError>>, GlobalLibraryWriterQueueError> {
        snapshot.validate_for_admission()?;
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

    /// Queue deletion of one exact profile vocabulary label at a matching revision.
    pub fn delete_profile_tag_vocabulary(
        &self,
        expected_revision: u64,
        label: String,
    ) -> Result<
        Receiver<Result<ProfileTagVocabularyDeleteReceipt, LibraryError>>,
        GlobalLibraryWriterQueueError,
    > {
        self.profile_tag_vocabulary_client()
            .delete_profile_tag_vocabulary(expected_revision, label)
    }

    /// Synchronously close/join the owner; returns whether closure was initiated here.
    ///
    /// This legacy boolean is not a successful-drain acknowledgement. Use
    /// `shutdown_with_report` for the retained terminal result or begin/poll for pending work.
    pub fn shutdown(&mut self) -> bool {
        let initiated = self.begin_shutdown();
        self.finish_shutdown();
        initiated
    }

    /// Read finite per-owner admission counters shared with vocabulary clients.
    /// Accepted counts mean queued, not committed; concurrent fields are approximate.
    pub fn admission_metrics(&self) -> GlobalAdmissionMetricsSnapshot {
        self.admission_metrics.snapshot()
    }

    fn ensure_available(&self) -> Result<(), GlobalLibraryWriterQueueError> {
        let result = ensure_available_state(&self.lifecycle, &self.unavailable);
        if result.is_err() {
            self.admission_metrics.record_outcome(&result);
        }
        result
    }

    fn send(&self, command: WriterCommand) -> Result<(), GlobalLibraryWriterQueueError> {
        send_command(
            &self.commands,
            &self.admission_gate,
            &self.lifecycle,
            &self.unavailable,
            &self.admission_metrics,
            command,
        )
    }
}

impl ProfileTagVocabularyClient {
    pub(super) fn read_harvest_facts(
        &self,
        request: HarvestFactsRequest,
    ) -> Result<Receiver<Result<HarvestFactsBatch, LibraryError>>, GlobalLibraryWriterQueueError>
    {
        let (result, receiver) = mpsc::sync_channel(1);
        self.send(WriterCommand::HarvestFacts { request, result })?;
        Ok(receiver)
    }

    /// Observe this handle's acquisition before forming a delayed proposal. Not authority.
    pub fn acquisition_id(&self) -> ProfileAcquisitionId {
        self.acquisition.clone()
    }

    /// Submit an already captured immutable proposal without substituting this handle's ID.
    /// The existing worker validates its retained capability before/after execution.
    pub fn submit_captured_mutation(
        &self,
        request: GlobalMutationRequest,
    ) -> Result<Receiver<GlobalMutationCompletion>, GlobalLibraryWriterQueueError> {
        if !matches!(
            request.context().kind(),
            GlobalMutationKind::VocabularySet | GlobalMutationKind::VocabularyDelete
        ) {
            return Err(GlobalLibraryWriterQueueError::InvalidMutationRoute);
        }
        self.ensure_available()?;
        let (result, receiver) = mpsc::sync_channel(1);
        self.send(WriterCommand::CapturedMutation { request, result })?;
        Ok(receiver)
    }

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

    /// Queue deletion of one exact profile vocabulary label at a matching revision.
    pub fn delete_profile_tag_vocabulary(
        &self,
        expected_revision: u64,
        label: String,
    ) -> Result<
        Receiver<Result<ProfileTagVocabularyDeleteReceipt, LibraryError>>,
        GlobalLibraryWriterQueueError,
    > {
        validate_delete_input(&label)?;
        self.ensure_available()?;
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        self.send(WriterCommand::DeleteProfileTagVocabulary {
            expected_revision,
            label,
            result: result_tx,
        })?;
        Ok(result_rx)
    }

    /// Read finite per-owner admission counters shared with vocabulary clients.
    /// Accepted counts mean queued, not committed; concurrent fields are approximate.
    pub fn admission_metrics(&self) -> GlobalAdmissionMetricsSnapshot {
        self.admission_metrics.snapshot()
    }

    fn ensure_available(&self) -> Result<(), GlobalLibraryWriterQueueError> {
        let result = ensure_available_state(&self.lifecycle, &self.unavailable);
        if result.is_err() {
            self.admission_metrics.record_outcome(&result);
        }
        result
    }

    fn send(&self, command: WriterCommand) -> Result<(), GlobalLibraryWriterQueueError> {
        send_command(
            &self.commands,
            &self.admission_gate,
            &self.lifecycle,
            &self.unavailable,
            &self.admission_metrics,
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
    commands: &Arc<Mutex<Option<SyncSender<QueuedCommand>>>>,
    admission_gate: &Arc<Mutex<()>>,
    lifecycle: &Arc<std::sync::atomic::AtomicU8>,
    unavailable: &Arc<Mutex<Option<GlobalLibraryWriterUnavailable>>>,
    metrics: &Arc<admission_metrics::AdmissionMetrics>,
    command: WriterCommand,
) -> Result<(), GlobalLibraryWriterQueueError> {
    let started = std::time::Instant::now();
    let admission = admission_gate
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let wait = started.elapsed();
    let result = (|| {
        ensure_available_state(lifecycle, unavailable)?;
        let sender = commands
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .cloned()
            .ok_or(GlobalLibraryWriterQueueError::Closed)?;
        let queued = QueuedCommand::new(command, Arc::clone(&metrics.dispatch));
        sender.try_send(queued).map_err(|error| match error {
            TrySendError::Full(_) => GlobalLibraryWriterQueueError::Full,
            TrySendError::Disconnected(_) => GlobalLibraryWriterQueueError::Closed,
        })
    })();
    // Telemetry subscribers never execute while the admission/shutdown gate is held.
    drop(admission);
    metrics.record_wait(wait);
    metrics.record_outcome(&result);
    result
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
    _global_writer_lease: ProfileChildLease,
    command_rx: Receiver<QueuedCommand>,
    lifecycle: Arc<std::sync::atomic::AtomicU8>,
    unavailable: Arc<Mutex<Option<GlobalLibraryWriterUnavailable>>>,
    hooks: OwnerHooks,
    admission_gate: Arc<Mutex<()>>,
) {
    if let Some(initialization_gate) = hooks.initialization_gate {
        initialization_gate.wait();
    }
    if WriterLifecycle::from_u8(lifecycle.load(std::sync::atomic::Ordering::Acquire))
        == WriterLifecycle::Closing
    {
        lifecycle.store(
            WriterLifecycle::Closed as u8,
            std::sync::atomic::Ordering::Release,
        );
        return;
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
    #[cfg(test)]
    if let Some(hook) = hooks.database_hook.as_ref() {
        hook(&database.connection);
    }
    // Shutdown may have begun while initialization owned its connection. Never reopen
    // admission/lifecycle after Closing; the existing receiver still drains accepted work.
    let _ = lifecycle.compare_exchange(
        WriterLifecycle::Initializing as u8,
        WriterLifecycle::Available as u8,
        std::sync::atomic::Ordering::AcqRel,
        std::sync::atomic::Ordering::Acquire,
    );

    let mut snapshot_fences = SnapshotFences::default();
    let mut harvest_generation = harvest_facts::Generation::new();
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
        let (command, dispatch) = match command_rx.recv_timeout(OWNER_POLL_INTERVAL) {
            Ok(queued) => queued.received(),
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

        dispatch.record();

        // Invalidate weak read results before a write attempt, including a failed/replayed
        // attempt. Only this actual retained worker owns a strong publication generation.
        if matches!(
            &command,
            WriterCommand::HarvestAuthored { .. }
                | WriterCommand::HarvestCopy { .. }
                | WriterCommand::ReferenceRekey { .. }
        ) {
            harvest_generation = harvest_facts::Generation::new();
        }
        let reply = execute_command(
            command,
            &mut database,
            profile_guard.owner_identity(),
            &mut snapshot_fences,
            &harvest_generation,
        );

        #[cfg(test)]
        if let Some((ready, release)) = hooks.post_command_gate.as_ref() {
            ready.wait();
            release.wait();
        }

        // The marker consumer validates ownership before maintenance; retain the existing
        // reply check afterwards so replacement during a checkpoint cannot be confirmed.
        database.maintain_after_ordinary_commit(&profile_guard);

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

fn execute_command(
    command: WriterCommand,
    database: &mut LibraryDatabase,
    owner_identity: &str,
    snapshot_fences: &mut SnapshotFences,
    harvest_generation: &Arc<harvest_facts::Generation>,
) -> WriterReply {
    match command {
        WriterCommand::HarvestFacts { request, result } => WriterReply::HarvestFacts {
            value: database.harvest_facts(owner_identity, &request, harvest_generation),
            request,
            result,
        },
        WriterCommand::HarvestAuthored { request, result } => WriterReply::HarvestAuthored {
            value: database.apply_harvest_authored(owner_identity, &request),
            request,
            result,
        },
        WriterCommand::HarvestAuthoredSnapshot { request, result } => {
            WriterReply::HarvestAuthoredSnapshot {
                value: database.harvest_authored_snapshot(owner_identity, &request),
                request,
                result,
            }
        }
        WriterCommand::HarvestCopy { request, result } => WriterReply::HarvestCopy {
            value: database.apply_harvest_copy(owner_identity, &request),
            request,
            result,
        },
        #[cfg(any(test, feature = "test-support"))]
        WriterCommand::PanicForTest { .. } => panic!("isolated global owner teardown fixture"),
        WriterCommand::CapturedMutation { request, result } => WriterReply::CapturedMutation {
            value: request.execute(database, owner_identity, snapshot_fences),
            result,
        },
        WriterCommand::ReferenceSnapshot { request, result } => WriterReply::ReferenceSnapshot {
            value: database.reference_snapshot(owner_identity, &request),
            request,
            result,
        },
        WriterCommand::ReferenceRekey { request, result } => WriterReply::ReferenceRekey {
            value: database.apply_reference_rekey(owner_identity, &request),
            request,
            result,
        },
        WriterCommand::ApplyOperationLink { request, result } => WriterReply::OperationLink {
            value: database.apply_operation_link(owner_identity, &request),
            result,
        },
        WriterCommand::ApplySourceRegistryParticipant { request, result } => {
            WriterReply::SourceRegistryParticipant {
                value: database.apply_source_registry_participant(owner_identity, &request),
                result,
            }
        }
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
        WriterCommand::DeleteProfileTagVocabulary {
            expected_revision,
            label,
            result,
        } => WriterReply::DeleteProfileTagVocabulary {
            value: database.delete_profile_tag_vocabulary(expected_revision, &label),
            result,
        },
    }
}

fn close_after_profile_change(
    command_rx: &Receiver<QueuedCommand>,
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
    command_rx: &Receiver<QueuedCommand>,
    error: &ProfileOwnershipError,
) {
    while let Ok(command) = command_rx.try_recv() {
        reject_after_profile_change(command.discard(), error);
    }
}

fn send_command_error(command: WriterCommand, error: LibraryError) {
    match command {
        WriterCommand::HarvestFacts { result, .. } => {
            let _ = result.send(Err(error));
        }
        WriterCommand::HarvestAuthored { result, .. } => {
            let _ = result.send(Err(error));
        }
        WriterCommand::HarvestAuthoredSnapshot { result, .. } => {
            let _ = result.send(Err(error));
        }
        WriterCommand::HarvestCopy { result, .. } => {
            let _ = result.send(Err(error));
        }
        #[cfg(any(test, feature = "test-support"))]
        WriterCommand::PanicForTest { result } => {
            let _ = result.send(Err(error));
        }
        WriterCommand::CapturedMutation { request, result } => {
            let _ = result.send(request.failed(error));
        }
        WriterCommand::ReferenceSnapshot { result, .. } => {
            let _ = result.send(Err(error));
        }
        WriterCommand::ReferenceRekey { result, .. } => {
            let _ = result.send(Err(error));
        }
        WriterCommand::ApplyOperationLink { result, .. } => {
            let _ = result.send(Err(error));
        }
        WriterCommand::ApplySourceRegistryParticipant { result, .. } => {
            let _ = result.send(Err(error));
        }

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
        WriterCommand::DeleteProfileTagVocabulary { result, .. } => {
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
        | ProfileOwnershipError::SourceRootReplaced { path }
        | ProfileOwnershipError::ProfileOwnedByAnotherProcess { path }
        | ProfileOwnershipError::Io { path, .. }
        | ProfileOwnershipError::NotRegularFile { path }
        | ProfileOwnershipError::IdentityUnavailable { path }
        | ProfileOwnershipError::ProfileChildAlreadyLeased { path }
        | ProfileOwnershipError::ExternalSourceRootUnavailable { path, .. }
        | ProfileOwnershipError::ExternalSourceRootIdentityUnavailable { path, .. }
        | ProfileOwnershipError::ProfileChildDatabaseRootUnavailable { path, .. }
        | ProfileOwnershipError::ProfileChildDatabaseRootReplaced { path }
        | ProfileOwnershipError::ProfileChildDatabaseRootIdentityUnavailable { path, .. }
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
    mod shutdown_tests {
        use super::*;
        include!("shutdown_owner_tests.rs");
    }
    mod snapshot_fence_tests {
        use super::*;
        include!("snapshot_fence_owner_tests.rs");
    }
    mod reference_tests {
        use super::*;
        include!("reference_owner_tests.rs");
        include!("reference_owner_tests/harvest_facts.rs");
    }
    mod mutation_context_tests {
        use super::*;
        include!("mutation_context_owner_tests.rs");
    }
    mod operation_link_tests {
        use super::*;
        include!("operation_link_owner_tests.rs");
    }
    mod global_replay_contract {
        use super::*;
        include!("global_replay_contract_tests.rs");
    }

    mod registry_participant_tests {
        use super::*;
        include!("registry_participant_owner_tests.rs");
    }

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
    fn source_registry_queue_rejects_oversized_snapshot_before_admission() {
        let writer = GlobalLibraryWriter::disabled();
        let sources = (0..=SOURCE_REGISTRY_MAX_DESCRIPTORS)
            .map(|index| source(Path::new("bounded/root"), &format!("source-{index}")))
            .collect();
        let error = writer
            .replace_source_registry(SourceRegistrySnapshot::new(sources))
            .expect_err("descriptor lookahead must reject before queue admission");
        assert!(matches!(
            error,
            GlobalLibraryWriterQueueError::SourceRegistryTooManyDescriptors {
                limit: SOURCE_REGISTRY_MAX_DESCRIPTORS,
                actual
            } if actual == SOURCE_REGISTRY_MAX_DESCRIPTORS + 1
        ));
        assert!(matches!(writer.load_source_registry_snapshot(), Ok(_)));
    }

    #[test]
    fn source_registry_queue_rejects_escaped_descriptor_over_limit() {
        let writer = GlobalLibraryWriter::disabled();
        let escaped = "\"".repeat(SOURCE_REGISTRY_MAX_DESCRIPTOR_BYTES / 2 + 1);
        let oversized = source(Path::new(&escaped), "escaped");
        let error = writer
            .replace_source_registry(SourceRegistrySnapshot::new(vec![oversized]))
            .expect_err("escaped JSON descriptor must fail closed");
        assert!(matches!(
            error,
            GlobalLibraryWriterQueueError::SourceRegistryFieldTooLarge { .. }
                | GlobalLibraryWriterQueueError::SourceRegistryPayloadTooLarge { .. }
        ));
    }

    #[test]
    fn source_registry_queue_rejects_aggregate_bytes_before_admission() {
        let writer = GlobalLibraryWriter::disabled();
        let root = PathBuf::from("x".repeat(60_000));
        let within_limit = (0..260)
            .map(|index| source(&root, &format!("source-{index}")))
            .collect();
        writer
            .replace_source_registry(SourceRegistrySnapshot::new(within_limit))
            .expect("bounded aggregate passes admission");

        let over_limit = (0..280)
            .map(|index| source(&root, &format!("source-{index}")))
            .collect();
        assert!(matches!(
            writer.replace_source_registry(SourceRegistrySnapshot::new(over_limit)),
            Err(
                GlobalLibraryWriterQueueError::SourceRegistryPayloadTooLarge {
                    limit: SOURCE_REGISTRY_MAX_BYTES,
                    ..
                }
            )
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
    fn second_writer_for_cloned_profile_guard_is_rejected_before_open() {
        let temp = tempdir().expect("base");
        let _runtime = TestRuntimeGuard::acquire();
        let base_path = std::fs::canonicalize(temp.path()).expect("canonical test base");
        let base_guard = ConfigBaseGuard::set(base_path);
        let profile_guard = PersistenceProfileGuard::named("single-global-writer");
        let writable_guard = WritableProfileGuard::acquire_current().expect("profile ownership");
        let mut writer = GlobalLibraryWriter::start(&writable_guard).expect("writer start");
        wait_until_available(&writer);
        assert!(matches!(
            GlobalLibraryWriter::start(&writable_guard),
            Err(GlobalLibraryWriterStartError::ProfileOwnership(
                ProfileOwnershipError::ProfileChildAlreadyLeased { .. }
            ))
        ));
        let cloned_guard = writable_guard.try_clone().expect("clone profile guard");

        assert!(matches!(
            GlobalLibraryWriter::start(&cloned_guard),
            Err(GlobalLibraryWriterStartError::ProfileOwnership(
                ProfileOwnershipError::ProfileChildAlreadyLeased { .. }
            ))
        ));
        assert_eq!(
            super::super::connection::test_open_count(
                &writable_guard
                    .profile_root()
                    .join(super::super::LIBRARY_DB_FILE_NAME)
            ),
            1
        );

        assert!(writer.shutdown());
        let mut replacement =
            GlobalLibraryWriter::start(&cloned_guard).expect("slot released after shutdown");
        wait_until_available(&replacement);
        assert_eq!(
            super::super::connection::test_open_count(
                &writable_guard
                    .profile_root()
                    .join(super::super::LIBRARY_DB_FILE_NAME)
            ),
            2
        );
        assert!(replacement.shutdown());
        drop(cloned_guard);
        drop(writable_guard);
        drop(profile_guard);
        drop(base_guard);
    }

    #[test]
    fn separate_profile_writer_slots_are_independent() {
        let temp = tempdir().expect("base");
        let _runtime = TestRuntimeGuard::acquire();
        let base_path = std::fs::canonicalize(temp.path()).expect("canonical test base");
        let base_guard = ConfigBaseGuard::set(base_path);

        let profile_a = PersistenceProfileGuard::named("independent-a");
        let writable_a = WritableProfileGuard::acquire_current().expect("profile A ownership");
        let mut writer_a = GlobalLibraryWriter::start(&writable_a).expect("writer A start");
        wait_until_available(&writer_a);

        let profile_b = PersistenceProfileGuard::named("independent-b");
        let writable_b = WritableProfileGuard::acquire_current().expect("profile B ownership");
        let mut writer_b = GlobalLibraryWriter::start(&writable_b).expect("writer B start");
        wait_until_available(&writer_b);
        assert!(matches!(
            writer_a.status(),
            GlobalLibraryWriterStatus::Available
        ));
        assert!(matches!(
            writer_b.status(),
            GlobalLibraryWriterStatus::Available
        ));

        assert!(writer_b.shutdown());
        assert!(writer_a.shutdown());
        drop(writable_b);
        drop(profile_b);
        drop(writable_a);
        drop(profile_a);
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

        let deleted = writer
            .delete_profile_tag_vocabulary(1, String::from("kick"))
            .expect("delete admission")
            .recv_timeout(Duration::from_secs(2))
            .expect("delete result")
            .expect("delete success");
        assert_eq!(deleted.revision, 2);
        assert_eq!(
            deleted.removed,
            Some(super::super::tag_vocabulary::ProfileTagVocabularyEntry {
                label: String::from("kick"),
                category: ProfileTagCategory::SoundType,
            })
        );

        let missing = writer
            .delete_profile_tag_vocabulary(2, String::from("kick"))
            .expect("missing delete admission")
            .recv_timeout(Duration::from_secs(2))
            .expect("missing delete result")
            .expect("missing delete success");
        assert_eq!(missing.revision, 2);
        assert!(missing.removed.is_none());

        let oversized = "x".repeat(super::super::PROFILE_TAG_VOCABULARY_MAX_LABEL_BYTES + 1);
        assert!(matches!(
            writer.set_profile_tag_category(1, oversized, ProfileTagCategory::Character),
            Err(GlobalLibraryWriterQueueError::InvalidProfileTagVocabulary(
                ProfileTagVocabularyInputError::LabelTooLong { .. }
            ))
        ));
        let oversized_delete = "x".repeat(super::super::PROFILE_TAG_VOCABULARY_MAX_LABEL_BYTES + 1);
        assert!(matches!(
            writer.delete_profile_tag_vocabulary(2, oversized_delete),
            Err(GlobalLibraryWriterQueueError::InvalidProfileTagVocabulary(
                ProfileTagVocabularyInputError::LabelTooLong { .. }
            ))
        ));
        assert!(matches!(
            writer.delete_profile_tag_vocabulary(2, String::from("bad\0label")),
            Err(GlobalLibraryWriterQueueError::InvalidProfileTagVocabulary(
                ProfileTagVocabularyInputError::LabelContainsNul
            ))
        ));
        assert!(matches!(
            writer.delete_profile_tag_vocabulary(2, String::from("Bad-label")),
            Err(GlobalLibraryWriterQueueError::InvalidProfileTagVocabulary(
                ProfileTagVocabularyInputError::LabelNotCanonical
            ))
        ));
        for label in ["loop", "oneshot", "one_shot", "one-shot"] {
            assert!(matches!(
                writer.delete_profile_tag_vocabulary(2, String::from(label)),
                Err(GlobalLibraryWriterQueueError::InvalidProfileTagVocabulary(
                    ProfileTagVocabularyInputError::LockedPlaybackLabelCannotBeDeleted
                ))
            ));
        }
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
    fn profile_vocabulary_delete_drains_after_result_receiver_is_dropped() {
        let temp = tempdir().expect("base");
        let _runtime = TestRuntimeGuard::acquire();
        let (base_guard, profile_guard, _writable_guard, mut writer) =
            start_writer(&temp, "profile-vocabulary-delete-drain");
        let _ = (&base_guard, &profile_guard);
        wait_until_available(&writer);

        writer
            .set_profile_tag_category(0, String::from("drop-me"), ProfileTagCategory::Character)
            .expect("set admission")
            .recv_timeout(Duration::from_secs(2))
            .expect("set result")
            .expect("set success");
        let dropped = writer
            .delete_profile_tag_vocabulary(1, String::from("drop-me"))
            .expect("delete admission");
        drop(dropped);
        assert!(writer.shutdown());

        let reopened = LibraryDatabase::open().expect("reopen library database");
        let lookup = reopened
            .lookup_profile_tag_categories(&[String::from("drop-me")])
            .expect("lookup deleted label");
        assert_eq!(lookup.revision, 2);
        assert!(lookup.entries.is_empty());
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
        let mut writer = GlobalLibraryWriter::start(&writable_guard).expect("writer start");
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
        let _ = writer.shutdown();
        let mut replacement = GlobalLibraryWriter::start(&writable_guard)
            .expect("failed initialization releases slot");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !matches!(
            replacement.status(),
            GlobalLibraryWriterStatus::Unavailable { .. }
        ) {
            assert!(
                Instant::now() < deadline,
                "replacement initialization timeout"
            );
            thread::sleep(Duration::from_millis(5));
        }
        let _ = replacement.shutdown();
        assert_eq!(super::super::connection::test_open_count(&path), 2);
        drop(replacement);
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
                .join(".yield")
                .join("profiles")
                .join("shutdown")
                .join("library.db"),
        )
        .expect("closed database is reopenable");
    }
}
