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
use yield_library::app_dirs::{
    AppRootGuard, PersistenceProfileGuard, ProfileOwnershipError, WritableProfileGuard,
};
use yield_library::operation_journal::{
    OperationJournalOwner, OperationJournalOwnerStatus, OperationJournalRecoveryReadiness,
    OperationJournalStore,
};
use yield_library::sample_sources::{
    GlobalLibraryWriter, GlobalLibraryWriterDrainOutcome, GlobalLibraryWriterQueueError,
    GlobalLibraryWriterStartError, GlobalLibraryWriterStatus, LibraryError, SampleSource, SourceId,
    SourceRegistrySnapshot, SourceRole,
};

use yield_library::sample_sources::library::{
    GlobalMutationCompletion, GlobalMutationContext, GlobalMutationKind, GlobalMutationRequest,
    GlobalMutationValue,
};

mod catalog;
mod maintenance;
mod preferences;
mod vocabulary;

pub use catalog::{
    CatalogCancelError, CatalogCommand, CatalogCurationRequestError,
    CatalogCurationTagFactsRequestError, CatalogDelivery, CatalogDisplayLabelsRequestError,
    CatalogError, CatalogEvent, CatalogHarvestFacts, CatalogIndexEntryHandle,
    CatalogIndexInspection, CatalogIndexPage, CatalogIndexPageCursor, CatalogIndexPageError,
    CatalogOperation, CatalogPublicationStatus, CatalogRatingDecayRequestError, CatalogStatus,
    CatalogSubmitError, CatalogTagAssignmentPreview, CatalogTagAssignmentPreviewSource,
    CatalogTagFilterRequestError, CatalogTagPageQuery, CatalogTagPageRequestError,
    CatalogWavMetadataRequestError, FeatureAnalysisBlocker, FeatureAnalysisCancellation,
    FeatureAnalysisPhase, FeatureAnalysisState, FeaturePhaseMeasurement, HARVEST_FACTS_MAX_BYTES,
    HARVEST_FACTS_MAX_PATH_BYTES, HARVEST_FACTS_MAX_PATHS, HarvestFactsCancellation,
    HarvestFactsEntry, HarvestFactsFailure, HarvestState, IndexInspectionCancellation,
    MAX_CURATION_OPERATION_BATCHES, MAX_CURATION_OPERATION_PATH_BYTES,
    MAX_CURATION_OPERATION_RECEIPT_BYTES, MAX_CURATION_OPERATION_UPDATES,
    MAX_CURATION_TAG_FACTS_PATH_BYTES, MAX_CURATION_UPDATES, MAX_HELD_INDEX_INSPECTIONS,
    MAX_INDEX_PAGE_PAYLOAD_BYTES, MAX_TAG_ASSIGNMENT_PREVIEW_BYTES,
    MAX_TAG_ASSIGNMENT_PREVIEW_PATHS, MAX_TAG_ASSIGNMENT_PREVIEW_SOURCES,
    MAX_TAG_FILTER_MASK_BYTES, MAX_TAG_FILTER_PAYLOAD_BYTES, MAX_TAG_PAGE_INPUT_BYTES,
    MAX_WAV_METADATA_REQUEST_PATHS, RetainedIndexCompleteness, SOURCE_CURATION_TAG_FACTS_MAX_PATHS,
    SOURCE_DISPLAY_LABEL_MAX_OUTPUT_BYTES, SOURCE_DISPLAY_LABEL_MAX_PATH_BYTES,
    SOURCE_DISPLAY_LABEL_MAX_PATHS, SOURCE_INDEX_PAGE_MAX_ENTRIES, SOURCE_TAG_PAGE_MAX_PAGE_SIZE,
    SOURCE_TAG_PAGE_MAX_PATH_BYTES, SOURCE_TAG_PAGE_MAX_PATHS, SOURCE_TAG_PAGE_MAX_PREFIX_BYTES,
    SOURCE_TAG_PAGE_MAX_TAG_IDS, SimilarityAspect, SimilarityCancellation,
    SimilarityCandidateOutcome, SimilarityDispatchFailure, SourceCurationCommitReceipt,
    SourceCurationOperationFailure, SourceCurationOperationFence, SourceCurationOperationOutcome,
    SourceCurationTagFacts, SourceCurationUpdate, SourceDisplayLabelEntry,
    SourceHarvestFactsRequest, SourceIndexInspectionError, SourceSimilarityRequest,
    SourceSimilarityResult, SourceSimilarityRow, SourceTagAssignmentCursor,
    SourceTagAssignmentPage, SourceTagPage, SourceTagPageCursor, SourceTagPageEntry,
    VerifiedSourceRoot, resolve_source_root,
};
use catalog::{CatalogService, CatalogSlot};
use maintenance::{
    MaintenanceAdmission, MaintenanceKind, MaintenanceReservation, MaintenanceService,
};
use vocabulary::VocabularySlot;

pub use maintenance::{
    CacheClearOutcome, GlobalStorageUsage, MaintenanceError, MaintenanceRequest,
};

pub use preferences::{
    AudioOutputPreferences, DEFAULT_RATING_DECAY_WEEKS, DEFAULT_TEXT_SCALE_PERCENT,
    ENLARGED_TEXT_SCALE_PERCENT, MAX_COLLECTION_NAME_BYTES, MAX_RATING_DECAY_WEEKS,
    MAX_USER_SHADER_SOURCE_BYTES, MAX_VISUAL_EFFECT_STRENGTH_PERCENT, MIN_RATING_DECAY_WEEKS,
    PreferenceError, PreferenceEvent, PreferencePatch, PreferenceRequestId, PreferenceRevision,
    ShippedVisualEffectPreferences, ShippedVisualEffectPreset, UI_PREFERENCES_SCHEMA_VERSION,
    USER_SHADER_CONTRACT_VERSION, UiPreferences, UserShaderPreferences,
    user_shader_content_fingerprint,
};
use preferences::{PreferenceCoordinator, PreferenceOutcome, PreferenceSaveDeadline};

pub use vocabulary::{
    MAX_VOCABULARY_REQUESTS_IN_FLIGHT, ProfileAcquisitionId, ProfileTagCategory,
    ProfileTagVocabularyDeleteReceipt, ProfileTagVocabularyEntry, ProfileTagVocabularyInputError,
    ProfileTagVocabularyLookup, ProfileTagVocabularyPage, ProfileTagVocabularyPageCursor,
    ProfileTagVocabularySetReceipt, VocabularyRequest, VocabularyRequestId, VocabularyStatus,
    VocabularySubmitError,
};

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
    /// The existing global owner could not confirm a normal shutdown drain.
    #[error("global library shutdown was unconfirmed: {0:?}")]
    GlobalWriterDrain(GlobalLibraryWriterDrainOutcome),
    /// Persistence could not be acquired or became unavailable.
    #[error("session persistence unavailable: {0}")]
    Persistence(String),
    /// Retained operation-journal data needs recovery before ordinary writable startup.
    #[error("session operation-journal recovery unavailable: {0}")]
    RecoveryUnavailable(String),
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
    /// A storage or cache maintenance operation is already admitted.
    #[error("a maintenance operation is already active")]
    MaintenanceActive,
    /// The service has closed command admission.
    #[error("source session service is closed")]
    Closed,
}

/// Errors returned when synchronously draining the session worker.
#[derive(Clone, Debug, Error)]
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
    maintenance_admission: Arc<MaintenanceAdmission>,
    next_request: AtomicU64,
    next_preference_request: AtomicU64,
    next_vocabulary_request: AtomicU64,
    events: Receiver<RegistryEvent>,
    preference_events: Receiver<PreferenceEvent>,
    catalog: Arc<Mutex<CatalogSlot>>,
    vocabulary: Arc<Mutex<VocabularySlot>>,
    worker: Option<JoinHandle<Result<(), SessionError>>>,
    shutdown_result: Option<Result<(), ShutdownError>>,
}

impl SourceRegistryService {
    /// Start a session on the fixed non-live `gpui-development` profile.
    pub fn start() -> Result<Self, StartError> {
        Self::start_inner(None)
    }

    /// Submit a source root for durable registration.
    pub fn add_root(&self, root: CanonicalSourceRoot) -> Result<RequestId, SubmitError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(SubmitError::Closed);
        }
        if self.maintenance_admission.clear_reserved() {
            return Err(SubmitError::MaintenanceActive);
        }
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

    /// Reorder one configured source relative to another at an exact registry revision.
    ///
    /// This changes only the persisted source-list order. The worker rejects stale revisions
    /// and unknown IDs before producing a replacement snapshot.
    pub fn reorder_source(
        &self,
        source_id: SourceId,
        target_id: SourceId,
        after: bool,
        expected_revision: RegistryRevision,
    ) -> Result<RequestId, SubmitError> {
        self.submit(|request| Command::ReorderSource {
            request,
            source_id,
            target_id,
            after,
            expected_revision,
        })
    }

    /// Set or clear a source primary role at an exact registry revision.
    /// Protected targets and registry changes not yet durably settled are rejected by the owner.
    /// This persists registry metadata only and grants no filesystem mutation authority.
    pub fn set_source_primary(
        &self,
        source_id: SourceId,
        primary: bool,
        expected_revision: RegistryRevision,
    ) -> Result<RequestId, SubmitError> {
        self.submit(|request| Command::SetSourcePrimary {
            request,
            source_id,
            primary,
            expected_revision,
        })
    }

    /// Submit a typed preference patch to the shared session owner.
    pub fn patch_preferences(
        &self,
        patch: PreferencePatch,
    ) -> Result<PreferenceRequestId, SubmitError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(SubmitError::Closed);
        }
        if matches!(&patch, PreferencePatch::SetTrashFolder(_))
            && self.maintenance_admission.clear_reserved()
        {
            return Err(SubmitError::MaintenanceActive);
        }
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

    /// Request a bounded measurement of the retained profile's database and cache footprint.
    pub fn request_storage_usage(
        &self,
    ) -> Result<MaintenanceRequest<GlobalStorageUsage>, SubmitError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(SubmitError::Closed);
        }
        let reservation = self
            .maintenance_admission
            .reserve(MaintenanceKind::StorageUsage)
            .ok_or(SubmitError::MaintenanceActive)?;
        let cancel = reservation.cancel_token();
        let generation = reservation.generation();
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        self.submit_command(Command::StorageUsage {
            reservation,
            result: result_tx,
        })?;
        Ok(MaintenanceRequest::new(result_rx, cancel, generation))
    }

    /// Request removal of entries in the captured profile-owned rebuildable cache namespace.
    pub fn clear_rebuildable_cache(
        &self,
    ) -> Result<MaintenanceRequest<CacheClearOutcome>, SubmitError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(SubmitError::Closed);
        }
        let reservation = self
            .maintenance_admission
            .reserve(MaintenanceKind::CacheClear)
            .ok_or(SubmitError::MaintenanceActive)?;
        let cancel = reservation.cancel_token();
        let generation = reservation.generation();
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        self.submit_command(Command::ClearRebuildableCache {
            reservation,
            result: result_tx,
        })?;
        Ok(MaintenanceRequest::new(result_rx, cancel, generation))
    }

    /// Return the latest monotonic generation assigned to a maintenance request.
    pub fn maintenance_generation(&self) -> u64 {
        self.maintenance_admission.generation()
    }

    /// Return the current lifecycle state of the session-facing profile tag vocabulary.
    pub fn vocabulary_status(&self) -> VocabularyStatus {
        vocabulary::status(&self.vocabulary)
    }

    /// Observe the vocabulary owner's acquisition before retaining a delayed UI intent.
    pub fn vocabulary_acquisition_id(
        &self,
    ) -> Result<yield_library::sample_sources::library::ProfileAcquisitionId, VocabularySubmitError>
    {
        vocabulary::acquisition_id(&self.vocabulary)
    }

    /// Submit a category proposal using its original acquisition, even through a fresh client.
    pub fn set_profile_tag_category_for_acquisition(
        &self,
        acquisition: yield_library::sample_sources::library::ProfileAcquisitionId,
        expected_revision: u64,
        label: String,
        category: ProfileTagCategory,
    ) -> Result<VocabularyRequest<ProfileTagVocabularySetReceipt>, VocabularySubmitError> {
        let request = VocabularyRequestId::from_raw(
            self.next_vocabulary_request.fetch_add(1, Ordering::Relaxed),
        );
        vocabulary::set_category(
            &self.vocabulary,
            request,
            Some(acquisition),
            expected_revision,
            label,
            category,
        )
    }

    /// Delete using the acquisition observed by the originating dictionary row or intent.
    pub fn delete_profile_tag_vocabulary_for_acquisition(
        &self,
        acquisition: yield_library::sample_sources::library::ProfileAcquisitionId,
        expected_revision: u64,
        label: String,
    ) -> Result<VocabularyRequest<ProfileTagVocabularyDeleteReceipt>, VocabularySubmitError> {
        let request = VocabularyRequestId::from_raw(
            self.next_vocabulary_request.fetch_add(1, Ordering::Relaxed),
        );
        vocabulary::delete_profile_tag_vocabulary(
            &self.vocabulary,
            request,
            Some(acquisition),
            expected_revision,
            label,
        )
    }

    /// Submit an exact lookup of bounded profile vocabulary labels.
    pub fn lookup_profile_tag_categories(
        &self,
        labels: Vec<String>,
    ) -> Result<VocabularyRequest<ProfileTagVocabularyLookup>, VocabularySubmitError> {
        let request = VocabularyRequestId::from_raw(
            self.next_vocabulary_request.fetch_add(1, Ordering::Relaxed),
        );
        vocabulary::lookup(&self.vocabulary, request, labels)
    }

    /// Submit one bounded literal-prefix profile vocabulary page.
    pub fn profile_tag_vocabulary_page(
        &self,
        expected_revision: Option<u64>,
        prefix: String,
        after_cursor: Option<ProfileTagVocabularyPageCursor>,
        requested_limit: usize,
    ) -> Result<VocabularyRequest<ProfileTagVocabularyPage>, VocabularySubmitError> {
        let request = VocabularyRequestId::from_raw(
            self.next_vocabulary_request.fetch_add(1, Ordering::Relaxed),
        );
        vocabulary::page(
            &self.vocabulary,
            request,
            expected_revision,
            prefix,
            after_cursor,
            requested_limit,
        )
    }

    /// Submit one category assignment at an exact profile vocabulary revision.
    pub fn set_profile_tag_category(
        &self,
        expected_revision: u64,
        label: String,
        category: ProfileTagCategory,
    ) -> Result<VocabularyRequest<ProfileTagVocabularySetReceipt>, VocabularySubmitError> {
        let request = VocabularyRequestId::from_raw(
            self.next_vocabulary_request.fetch_add(1, Ordering::Relaxed),
        );
        vocabulary::set_category(
            &self.vocabulary,
            request,
            None,
            expected_revision,
            label,
            category,
        )
    }

    /// Submit deletion of one exact profile vocabulary label at a matching revision.
    pub fn delete_profile_tag_vocabulary(
        &self,
        expected_revision: u64,
        label: String,
    ) -> Result<VocabularyRequest<ProfileTagVocabularyDeleteReceipt>, VocabularySubmitError> {
        let request = VocabularyRequestId::from_raw(
            self.next_vocabulary_request.fetch_add(1, Ordering::Relaxed),
        );
        vocabulary::delete_profile_tag_vocabulary(
            &self.vocabulary,
            request,
            None,
            expected_revision,
            label,
        )
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

    /// Cancel an exact retained-index query, invalidating its handles and queued inspections.
    pub fn cancel_catalog_index_query(&self, source_id: &SourceId, generation: u64, query_id: u64) {
        catalog::cancel_index_query(&self.catalog, source_id, generation, query_id);
    }

    /// Request cancellation of the exact current source audit without blocking on the worker.
    pub fn cancel_catalog_audit(
        &self,
        source_id: &SourceId,
        lifecycle_generation: u64,
    ) -> Result<(), CatalogCancelError> {
        catalog::cancel_audit(&self.catalog, source_id, lifecycle_generation)
    }

    /// Check whether the exact source lifecycle currently has an active audit to cancel.
    pub fn catalog_audit_is_running(
        &self,
        source_id: &SourceId,
        lifecycle_generation: u64,
    ) -> bool {
        catalog::audit_is_running(&self.catalog, source_id, lifecycle_generation)
    }

    /// Poll one catalog event without waiting for startup or worker I/O.
    pub fn try_recv_catalog(&self) -> Result<CatalogEvent, TryRecvError> {
        catalog::try_recv_event(&self.catalog)
    }

    /// Poll one catalog delivery with its retained source-publication fence.
    pub fn try_recv_catalog_delivery(&self) -> Result<CatalogDelivery, TryRecvError> {
        catalog::try_recv_delivery(&self.catalog)
    }

    /// Close command admission, drain the final baseline/lookups/save, and synchronously join.
    pub fn shutdown(&mut self) -> Result<(), ShutdownError> {
        if let Some(result) = &self.shutdown_result {
            return result.clone();
        }
        self.closed.store(true, Ordering::Release);
        vocabulary::close(&self.vocabulary);
        catalog::close(&self.catalog);
        let sender = self
            .commands
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        let acknowledgement = if let Some(sender) = sender {
            let (ack_tx, ack_rx) = mpsc::sync_channel(1);
            let submitted = sender
                .send(Command::Shutdown {
                    acknowledgement: ack_tx,
                })
                .map_err(|error| ShutdownError::WorkerChannel(error.to_string()));
            drop(sender);
            submitted.and_then(|()| {
                ack_rx
                    .recv()
                    .map_err(|error| ShutdownError::WorkerChannel(error.to_string()))?
                    .map_err(ShutdownError::Session)
            })
        } else {
            Ok(())
        };
        // Even an acknowledgement-channel failure must join the worker and retain its real
        // failure before releasing profile/journal custody. Never turn a second call into Ok.
        let joined = match self.worker.take() {
            Some(worker) => match worker.join() {
                Ok(result) => result.map_err(ShutdownError::Session),
                Err(_) => Err(ShutdownError::WorkerPanicked),
            },
            None => Ok(()),
        };
        let result = joined.and(acknowledgement);
        self.shutdown_result = Some(result.clone());
        result
    }

    fn start_inner(app_root_override: Option<PathBuf>) -> Result<Self, StartError> {
        Self::start_inner_with_initialization_gate(
            app_root_override,
            None,
            #[cfg(test)]
            false,
        )
    }

    /// Start a portable registry fixture with real profile/SQLite ownership but no reserve.
    /// This seam is compiled only in this crate's tests; normal startup remains qualified.
    #[cfg(test)]
    fn start_portable_registry_for_test(app_root: PathBuf) -> Result<Self, StartError> {
        Self::start_inner_with_initialization_gate(Some(app_root), None, true)
    }

    /// Gate the real owner within the portable metadata-only test fixture.
    #[cfg(test)]
    fn start_portable_vocabulary_with_gate_for_test(
        app_root: PathBuf,
        initialization_gate: Arc<std::sync::Barrier>,
    ) -> Result<Self, StartError> {
        Self::start_inner_with_initialization_gate(Some(app_root), Some(initialization_gate), true)
    }

    #[cfg(test)]
    fn start_inner_with_initialization_gate_for_test(
        app_root_override: Option<PathBuf>,
        initialization_gate: Arc<std::sync::Barrier>,
    ) -> Result<Self, StartError> {
        Self::start_inner_with_initialization_gate(
            app_root_override,
            Some(initialization_gate),
            false,
        )
    }

    fn start_inner_with_initialization_gate(
        app_root_override: Option<PathBuf>,
        initialization_gate: Option<Arc<std::sync::Barrier>>,
        #[cfg(test)] portable_registry_fixture: bool,
    ) -> Result<Self, StartError> {
        let (command_tx, command_rx) = mpsc::sync_channel(SESSION_COMMAND_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel();
        let (preference_event_tx, preference_event_rx) = mpsc::channel();
        let catalog = Arc::new(Mutex::new(CatalogSlot::loading()));
        let worker_catalog = Arc::clone(&catalog);
        let vocabulary = Arc::new(Mutex::new(VocabularySlot::loading()));
        let worker_vocabulary = Arc::clone(&vocabulary);
        let maintenance_admission = Arc::new(MaintenanceAdmission::new());
        let worker_maintenance_admission = Arc::clone(&maintenance_admission);
        let worker = thread::Builder::new()
            .name(String::from("yield-source-session"))
            .spawn(move || {
                run_worker(
                    command_rx,
                    event_tx,
                    preference_event_tx,
                    app_root_override,
                    worker_catalog,
                    worker_vocabulary,
                    worker_maintenance_admission,
                    initialization_gate,
                    #[cfg(test)]
                    portable_registry_fixture,
                )
            })
            .map_err(StartError::ThreadSpawn)?;
        Ok(Self {
            commands: Mutex::new(Some(command_tx)),
            closed: AtomicBool::new(false),
            maintenance_admission,
            next_request: AtomicU64::new(1),
            next_preference_request: AtomicU64::new(1),
            next_vocabulary_request: AtomicU64::new(1),
            events: event_rx,
            preference_events: preference_event_rx,
            catalog,
            vocabulary,
            worker: Some(worker),
            shutdown_result: None,
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
    #[cfg(test)]
    PanicForTest,
    SetSourcePrimary {
        request: RequestId,
        source_id: SourceId,
        primary: bool,
        expected_revision: RegistryRevision,
    },
    AddRoot {
        request: RequestId,
        root: CanonicalSourceRoot,
    },
    RemoveRoot {
        request: RequestId,
        root: CanonicalSourceRoot,
    },
    ReorderSource {
        request: RequestId,
        source_id: SourceId,
        target_id: SourceId,
        after: bool,
        expected_revision: RegistryRevision,
    },
    PatchPreferences {
        request: PreferenceRequestId,
        patch: PreferencePatch,
    },
    StorageUsage {
        reservation: MaintenanceReservation,
        result: SyncSender<Result<GlobalStorageUsage, MaintenanceError>>,
    },
    ClearRebuildableCache {
        reservation: MaintenanceReservation,
        result: SyncSender<Result<CacheClearOutcome, MaintenanceError>>,
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
    Primary {
        request: RequestId,
        source_id: SourceId,
        primary: bool,
    },
    Add {
        request: RequestId,
        root: CanonicalSourceRoot,
        source: Option<SampleSource>,
    },
    Remove {
        request: RequestId,
        root: CanonicalSourceRoot,
    },
    Reorder {
        request: RequestId,
        source_id: SourceId,
        target_id: SourceId,
        after: bool,
    },
}

struct SaveInFlight {
    revision: RegistryRevision,
    context: GlobalMutationContext,
    receiver: Receiver<GlobalMutationCompletion>,
}

struct PreferenceSaveInFlight {
    revision: PreferenceRevision,
    context: GlobalMutationContext,
    receiver: Receiver<GlobalMutationCompletion>,
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

    fn reorder_edit(
        &mut self,
        request: RequestId,
        source_id: SourceId,
        target_id: SourceId,
        after: bool,
    ) {
        self.edits.push(Edit::Reorder {
            request,
            source_id,
            target_id,
            after,
        });
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
                Edit::Reorder {
                    source_id,
                    target_id,
                    after,
                    ..
                } => reorder_sources(&mut next, source_id, target_id, *after)?,
                Edit::Primary {
                    source_id, primary, ..
                } => set_primary_source(&mut next, source_id, *primary)?,
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

    fn maintenance_source_roots(&self) -> Vec<PathBuf> {
        let mut roots = self
            .current
            .iter()
            .map(|source| source.root.clone())
            .collect::<Vec<_>>();
        roots.extend(self.edits.iter().filter_map(|edit| match edit {
            Edit::Add {
                root, source: None, ..
            } => Some(root.as_path().to_path_buf()),
            Edit::Add { .. }
            | Edit::Remove { .. }
            | Edit::Reorder { .. }
            | Edit::Primary { .. } => None,
        }));
        roots.sort();
        roots.dedup();
        roots
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
            Edit::Add { source: None, .. }
            | Edit::Remove { .. }
            | Edit::Reorder { .. }
            | Edit::Primary { .. } => None,
        })
    }

    fn remove_requests(&self) -> impl Iterator<Item = RequestId> + '_ {
        self.edits.iter().filter_map(|edit| match edit {
            Edit::Remove { request, .. } => Some(*request),
            Edit::Add { .. } | Edit::Reorder { .. } | Edit::Primary { .. } => None,
        })
    }

    fn discard_requests(&mut self, requests: &HashSet<RequestId>) -> Result<(), SessionError> {
        if requests.is_empty() {
            return Ok(());
        }
        self.edits.retain(|edit| {
            let request = match edit {
                Edit::Add { request, .. }
                | Edit::Remove { request, .. }
                | Edit::Reorder { request, .. }
                | Edit::Primary { request, .. } => request,
            };
            !requests.contains(request)
        });
        self.rebuild().map(|_| ())
    }
}

fn set_primary_source(
    sources: &mut [SampleSource],
    source_id: &SourceId,
    primary: bool,
) -> Result<(), SessionError> {
    let matches = sources
        .iter()
        .filter(|source| &source.id == source_id)
        .count();
    if matches != 1
        || sources
            .iter()
            .any(|source| &source.id == source_id && source.role == SourceRole::Protected)
    {
        return Err(SessionError::Library(String::from(
            "primary source requires one current writable source ID",
        )));
    }
    for source in sources {
        if &source.id == source_id {
            source.role = if primary {
                SourceRole::Primary
            } else {
                SourceRole::Normal
            };
        } else if primary && source.role == SourceRole::Primary {
            source.role = SourceRole::Normal;
        }
    }
    Ok(())
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

fn reorder_sources(
    sources: &mut Vec<SampleSource>,
    source_id: &SourceId,
    target_id: &SourceId,
    after: bool,
) -> Result<(), SessionError> {
    let source_matches = sources
        .iter()
        .enumerate()
        .filter_map(|(index, source)| (&source.id == source_id).then_some(index))
        .collect::<Vec<_>>();
    let target_matches = sources
        .iter()
        .enumerate()
        .filter_map(|(index, source)| (&source.id == target_id).then_some(index))
        .collect::<Vec<_>>();
    if source_matches.len() != 1 || target_matches.len() != 1 {
        return Err(SessionError::Library(String::from(
            "source reorder requires two current unique source IDs",
        )));
    }
    if source_id == target_id {
        return Ok(());
    }
    let moved = sources.remove(source_matches[0]);
    let target = sources
        .iter()
        .position(|source| &source.id == target_id)
        .expect("validated target source remains after removing a different ID");
    sources.insert(target + usize::from(after), moved);
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
    maintenance: MaintenanceService,
    catalog_slot: Arc<Mutex<CatalogSlot>>,
    vocabulary_slot: Arc<Mutex<VocabularySlot>>,
    catalog: Option<Arc<CatalogService>>,
    writer: Option<GlobalLibraryWriter>,
    journal: Option<OperationJournalOwner>,
    writable: Option<WritableProfileGuard>,
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
        vocabulary_slot: Arc<Mutex<VocabularySlot>>,
        maintenance_admission: Arc<MaintenanceAdmission>,
        #[cfg(test)] portable_registry_fixture: bool,
    ) -> (Self, Option<AppRootGuard>, PersistenceProfileGuard) {
        #[cfg(test)]
        let fixture_registry = app_root_override
            .as_ref()
            .filter(|_| !portable_registry_fixture)
            .and_then(|root| {
                root.parent()
                    .and_then(Path::parent)
                    .map(|app_root| app_root.join("volume-registry"))
            });
        let profile_guard = PersistenceProfileGuard::named(GPUI_DEVELOPMENT_PROFILE);
        let mut persistence = Self {
            writable: None,
            maintenance: MaintenanceService::new(maintenance_admission),
            catalog_slot: Arc::clone(&catalog_slot),
            vocabulary_slot: Arc::clone(&vocabulary_slot),
            writer: None,
            catalog: None,
            journal: None,
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
        #[cfg(test)]
        if persistence.unavailable.is_none() {
            if let Some(registry) = fixture_registry.as_ref() {
                if let Err(error) = std::fs::create_dir_all(registry) {
                    persistence.unavailable = Some(SessionError::Persistence(format!(
                        "session fixture volume registry unavailable: {error}"
                    )));
                }
            }
        }
        if persistence.unavailable.is_none() {
            match WritableProfileGuard::acquire_current() {
                Ok(writable) => match open_session_journal(&writable) {
                    Ok(journal) => match {
                        #[cfg(test)]
                        {
                            if portable_registry_fixture {
                                OperationJournalOwner::start_without_capacity_for_test(journal)
                            } else {
                                match fixture_registry {
                                    Some(registry) => {
                                        OperationJournalOwner::start_with_capacity_fixture_for_test(
                                            journal, registry,
                                        )
                                    }
                                    None => OperationJournalOwner::start(journal),
                                }
                            }
                        }
                        #[cfg(not(test))]
                        {
                            OperationJournalOwner::start(journal)
                        }
                    } {
                        Ok(journal) => match GlobalLibraryWriter::start(&writable) {
                            Ok(writer) => {
                                vocabulary::install(
                                    &vocabulary_slot,
                                    writer.profile_tag_vocabulary_client(),
                                );
                                persistence.writable = Some(writable);
                                persistence.journal = Some(journal);
                                persistence.writer = Some(writer);
                            }
                            Err(error) => {
                                persistence.unavailable = Some(session_from_start_error(error))
                            }
                        },
                        Err(error) => {
                            persistence.unavailable = Some(SessionError::Persistence(format!(
                                "operation journal owner unavailable: {error}"
                            )))
                        }
                    },
                    Err(error) => persistence.unavailable = Some(error),
                },
                Err(error) => persistence.unavailable = Some(session_from_profile_error(error)),
            }
        }
        if let Some(error) = persistence.unavailable.as_ref() {
            vocabulary::mark_unavailable(&vocabulary_slot, error.to_string());
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
        let journal = self.journal.as_ref().map(OperationJournalOwner::client);
        let service_result = match self.writer.as_ref() {
            Some(writer) => CatalogService::start_with_profile_owners(
                writable,
                journal,
                writer.harvest_facts_client(),
            ),
            None => match journal {
                Some(journal) => CatalogService::start_with_journal(writable, journal),
                None => CatalogService::start(writable),
            },
        };
        let service = match service_result {
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
        self.unavailable
            .clone()
            .or_else(|| {
                self.journal
                    .as_ref()
                    .and_then(|journal| match journal.status() {
                        OperationJournalOwnerStatus::Unavailable { reason } => {
                            Some(SessionError::Persistence(format!(
                                "operation journal owner unavailable: {reason}"
                            )))
                        }
                        OperationJournalOwnerStatus::Closed => Some(SessionError::Persistence(
                            String::from("operation journal owner closed"),
                        )),
                        _ => None,
                    })
            })
            .or_else(|| {
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

    fn fence_dependents_after_journal_failure(&self, error: &SessionError) {
        if !self.journal.as_ref().is_some_and(|journal| {
            matches!(
                journal.status(),
                OperationJournalOwnerStatus::Unavailable { .. }
            )
        }) {
            return;
        }
        let reason = error.to_string();
        catalog::mark_unavailable(&self.catalog_slot, reason.clone());
        vocabulary::mark_unavailable(&self.vocabulary_slot, reason);
    }

    fn available(&self) -> bool {
        self.journal.as_ref().is_some_and(|journal| {
            matches!(journal.status(), OperationJournalOwnerStatus::Available)
        }) && self
            .writer
            .as_ref()
            .is_some_and(|writer| matches!(writer.status(), GlobalLibraryWriterStatus::Available))
    }

    fn shutdown(&mut self) -> Result<(), SessionError> {
        self.maintenance.shutdown();
        if let Some(catalog) = self.catalog.take() {
            catalog.shutdown();
        }
        let global_result = if let Some(mut writer) = self.writer.take() {
            let report = writer.shutdown_with_report();
            match report.outcome() {
                GlobalLibraryWriterDrainOutcome::Drained => Ok(()),
                outcome => Err(SessionError::GlobalWriterDrain(outcome.clone())),
            }
        } else {
            Ok(())
        };
        self.save = None;
        self.preference_load = None;
        self.preference_save = None;
        self.preference_save_deadline.clear();
        self.load = None;
        self.pending_adds.clear();
        self.preference_unavailable = None;
        // The journal worker owns a cloned profile guard and child lease. Stop it after all
        // ordinary writers and catalog/maintenance workers have stopped, but before the session's
        // final profile guard is dropped.
        if let Some(mut journal) = self.journal.take() {
            let _ = journal.shutdown();
        }
        self.writable.take();
        global_result
    }
}

impl Drop for Persistence {
    fn drop(&mut self) {
        // Keep the shutdown ordering even when the worker exits through an early return or an
        // unwind. Catalog and global-writer workers must stop before the journal's retained child
        // lease and profile clone are released.
        let _ = self.shutdown();
    }
}

fn open_session_journal(
    writable: &WritableProfileGuard,
) -> Result<OperationJournalStore, SessionError> {
    let journal = OperationJournalStore::open(writable).map_err(|error| {
        SessionError::Persistence(format!("operation journal unavailable: {error}"))
    })?;
    let readiness = journal.recovery_readiness().map_err(|error| match error {
        yield_library::operation_journal::RecoveryScanError::Store(store_error) => {
            SessionError::RecoveryUnavailable(format!(
                "operation journal audit required: {store_error}"
            ))
        }
        yield_library::operation_journal::RecoveryScanError::RequiresAudit { reason } => {
            SessionError::RecoveryUnavailable(format!("operation journal audit required: {reason}"))
        }
    })?;
    if let OperationJournalRecoveryReadiness::Blocked { reason } = readiness {
        return Err(SessionError::RecoveryUnavailable(format!(
            "operation journal entries require recovery before writable access can resume: {reason}"
        )));
    }
    Ok(journal)
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
    vocabulary_slot: Arc<Mutex<VocabularySlot>>,
    maintenance_admission: Arc<MaintenanceAdmission>,
    initialization_gate: Option<Arc<std::sync::Barrier>>,
    #[cfg(test)] portable_registry_fixture: bool,
) -> Result<(), SessionError> {
    if let Some(initialization_gate) = initialization_gate {
        initialization_gate.wait();
    }
    let (mut persistence, _root_guard, _profile_guard) = Persistence::start(
        app_root_override,
        Arc::clone(&catalog_slot),
        Arc::clone(&vocabulary_slot),
        maintenance_admission,
        #[cfg(test)]
        portable_registry_fixture,
    );
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
            vocabulary::close(&vocabulary_slot);
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
            let result = result.and(persistence.shutdown());
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
                if persistence.maintenance.clear_reserved() {
                    fail_request(
                        &event_tx,
                        &mut unfinalized_requests,
                        request,
                        SessionError::Library(String::from(
                            "source registration paused while cache cleanup is active",
                        )),
                    );
                    continue;
                }
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
            Command::ReorderSource {
                request,
                source_id,
                target_id,
                after,
                expected_revision,
            } => {
                if !coordinator.loaded
                    || !persistence.pending_adds.is_empty()
                    || coordinator.revision() != expected_revision
                {
                    fail_request(
                        &event_tx,
                        &mut unfinalized_requests,
                        request,
                        SessionError::Library(String::from(
                            "source reorder rejected because the registry changed or is still loading",
                        )),
                    );
                    continue;
                }
                coordinator.reorder_edit(request, source_id, target_id, after);
                match coordinator.rebuild() {
                    Ok(changed) => {
                        if changed {
                            let _ = event_tx.send(RegistryEvent::Snapshot {
                                revision: coordinator.revision(),
                                snapshot: coordinator.snapshot(),
                            });
                        }
                        announce_request(
                            &event_tx,
                            &mut applied_requests,
                            &mut unfinalized_requests,
                            request,
                            coordinator.revision(),
                        );
                    }
                    Err(error) => {
                        let _ = coordinator.discard_requests(&HashSet::from([request]));
                        fail_request(&event_tx, &mut unfinalized_requests, request, error);
                    }
                }
                coordinator.compact_if_resolved(false);
            }
            Command::SetSourcePrimary {
                request,
                source_id,
                primary,
                expected_revision,
            } => {
                if !coordinator.loaded
                    || !persistence.pending_adds.is_empty()
                    || coordinator.is_dirty()
                    || persistence.save.is_some()
                    || coordinator.revision() != expected_revision
                {
                    fail_request(
                        &event_tx,
                        &mut unfinalized_requests,
                        request,
                        SessionError::Library(String::from(
                            "primary source change rejected because the registry changed or is not durably settled",
                        )),
                    );
                    continue;
                }
                coordinator.edits.push(Edit::Primary {
                    request,
                    source_id,
                    primary,
                });
                match coordinator.rebuild() {
                    Ok(changed) => {
                        if changed {
                            let _ = event_tx.send(RegistryEvent::Snapshot {
                                revision: coordinator.revision(),
                                snapshot: coordinator.snapshot(),
                            });
                        }
                        announce_request(
                            &event_tx,
                            &mut applied_requests,
                            &mut unfinalized_requests,
                            request,
                            coordinator.revision(),
                        );
                    }
                    Err(error) => {
                        let _ = coordinator.discard_requests(&HashSet::from([request]));
                        fail_request(&event_tx, &mut unfinalized_requests, request, error);
                    }
                }
                coordinator.compact_if_resolved(false);
            }
            Command::PatchPreferences { request, patch } => {
                if matches!(&patch, PreferencePatch::SetTrashFolder(_))
                    && persistence.maintenance.clear_reserved()
                {
                    fail_preference_request(
                        &preference_event_tx,
                        &mut unfinalized_preference_requests,
                        request,
                        SessionError::Preference(String::from(
                            "trash-folder changes are paused while cache cleanup is active",
                        )),
                    );
                    continue;
                }
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
            Command::StorageUsage {
                reservation,
                result,
            } => {
                persistence.maintenance.start_storage(
                    persistence.writable.as_ref(),
                    reservation,
                    result,
                );
            }
            Command::ClearRebuildableCache {
                reservation,
                result,
            } => {
                if !coordinator.loaded || !preference_coordinator.loaded() {
                    let _ = result.send(Err(MaintenanceError::NotReady));
                    drop(reservation);
                    continue;
                }
                let source_roots = coordinator.maintenance_source_roots();
                let preferences = preference_coordinator.snapshot();
                persistence.maintenance.start_clear(
                    persistence.writable.as_ref(),
                    &source_roots,
                    preferences.trash_folder.as_deref(),
                    reservation,
                    result,
                );
            }
            #[cfg(test)]
            Command::PanicForTest => panic!("session owner panic fixture"),
            Command::Shutdown { acknowledgement } => {
                vocabulary::close(&vocabulary_slot);
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
                let result = result.and(persistence.shutdown());
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
        #[cfg(test)]
        Command::PanicForTest => None,
        Command::AddRoot { request, .. }
        | Command::RemoveRoot { request, .. }
        | Command::ReorderSource { request, .. }
        | Command::SetSourcePrimary { request, .. } => Some(*request),
        Command::PatchPreferences { .. }
        | Command::StorageUsage { .. }
        | Command::ClearRebuildableCache { .. }
        | Command::Shutdown { .. } => None,
    }
}

fn preference_command_request(command: &Command) -> Option<PreferenceRequestId> {
    match command {
        #[cfg(test)]
        Command::PanicForTest => None,
        Command::PatchPreferences { request, .. } => Some(*request),
        Command::AddRoot { .. }
        | Command::RemoveRoot { .. }
        | Command::ReorderSource { .. }
        | Command::SetSourcePrimary { .. }
        | Command::StorageUsage { .. }
        | Command::ClearRebuildableCache { .. }
        | Command::Shutdown { .. } => None,
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
    let proposal = match GlobalMutationRequest::preferences(
        writer.acquisition_id(),
        coordinator.revision().get(),
        payload,
    ) {
        Ok(proposal) => proposal,
        Err(error) => {
            persistence.preference_unavailable = Some(SessionError::Preference(error.to_string()));
            return;
        }
    };
    let context = proposal.context().clone();
    match writer.submit_captured_mutation(proposal) {
        Ok(receiver) => {
            persistence.preference_save = Some(PreferenceSaveInFlight {
                revision: coordinator.revision(),
                context,
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

fn snapshot_completion_result(
    completion: GlobalMutationCompletion,
    context: &GlobalMutationContext,
    kind: GlobalMutationKind,
) -> Result<(), LibraryError> {
    match (kind, completion.into_result_for(context)?) {
        (GlobalMutationKind::ConfiguredRegistry, GlobalMutationValue::RegistryCommitted)
        | (GlobalMutationKind::UiPreferences, GlobalMutationValue::PreferencesCommitted) => Ok(()),
        _ => Err(LibraryError::GlobalMutationCompletionMismatch),
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
    let result = save.receiver.try_recv().map(|completion| {
        snapshot_completion_result(completion, &save.context, GlobalMutationKind::UiPreferences)
    });
    match result {
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
            LibraryError::ProfileOwnership(_)
                | LibraryError::ProfileOwnershipChanged { .. }
                | LibraryError::GlobalMutationStaleAcquisition
                | LibraryError::GlobalMutationCompletionMismatch
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
    let proposal = match GlobalMutationRequest::registry(
        writer.acquisition_id(),
        coordinator.revision().get(),
        coordinator.snapshot(),
    ) {
        Ok(proposal) => proposal,
        Err(error) => {
            persistence.unavailable = Some(session_from_queue_error(error));
            return;
        }
    };
    let context = proposal.context().clone();
    match writer.submit_captured_mutation(proposal) {
        Ok(receiver) => {
            persistence.save = Some(SaveInFlight {
                revision: coordinator.revision(),
                context,
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
    let result = save.receiver.try_recv().map(|completion| {
        snapshot_completion_result(
            completion,
            &save.context,
            GlobalMutationKind::ConfiguredRegistry,
        )
    });
    match result {
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
    persistence.fence_dependents_after_journal_failure(&error);
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
    mod harvest_catalog_tests {
        use super::*;
        include!("harvest_catalog_tests.rs");
    }
    mod snapshot_persistence {
        use super::*;
        include!("snapshot_persistence_tests.rs");
    }
    use super::*;
    use rusqlite::Connection;
    use std::{
        fs,
        io::{self, BufRead, BufReader, Write},
        process::{Child, ChildStdin, Command, Stdio},
        sync::mpsc::Receiver,
        time::Instant,
    };
    use tempfile::TempDir;
    use yield_library::app_dirs::{
        AppRootGuard, ConfigBaseGuard, PROFILE_DIR_NAME, ProfileOwnershipError,
        WritableProfileGuard,
    };
    use yield_library::operation_journal::{
        JOURNAL_DATABASE_FILE_NAME, JOURNAL_DATABASE_ROOT_NAME, OperationId, OperationJournalStore,
        decode,
    };
    use yield_library::sample_sources::{SourceId, SourceMetadataStorage};

    const SESSION_OWNER_PROFILE_ROOT_ENV: &str = "YIELD_SESSION_OWNER_PROFILE_ROOT";
    const SESSION_OWNER_READY_LINE: &str = "YIELD_SESSION_OWNER_READY";
    const FULL_PROGRESS_FIXTURE_OPERATION_ID: &str = "00000000-0000-0000-0000-0000000000cd";
    const FULL_PROGRESS_FIXTURE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../test-fixtures/full_progress_record.json"
    ));

    struct SessionOwnerHelperProcess {
        child: Child,
        stdin: Option<ChildStdin>,
        ready_rx: Receiver<Result<(), String>>,
        eof_rx: Receiver<()>,
    }

    impl Drop for SessionOwnerHelperProcess {
        fn drop(&mut self) {
            if self.child.try_wait().ok().flatten().is_none() {
                let _ = self.child.kill();
            }
            let _ = self.child.wait();
        }
    }

    fn spawn_session_owner_helper(profile_root: &Path) -> SessionOwnerHelperProcess {
        let mut command = Command::new(std::env::current_exe().expect("current test executable"));
        command
            .arg("--exact")
            .arg("tests::session_owner_process_helper")
            .arg("--ignored")
            .arg("--nocapture")
            .env(SESSION_OWNER_PROFILE_ROOT_ENV, profile_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("spawn session owner helper");
        let stdin = child.stdin.take().expect("session helper stdin");
        let stdout = child.stdout.take().expect("session helper stdout");
        let stderr = child.stderr.take().expect("session helper stderr");
        let (ready_tx, ready_rx) = mpsc::channel();
        let (eof_tx, eof_rx) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let ready = loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => {
                        break Err(String::from("session owner helper exited before handshake"));
                    }
                    Ok(_) if line.trim_end().ends_with(SESSION_OWNER_READY_LINE) => break Ok(()),
                    Ok(_) => continue,
                    Err(error) => {
                        break Err(format!("failed to read session owner handshake: {error}"));
                    }
                }
            };
            let _ = ready_tx.send(ready);
            let _ = io::copy(&mut reader, &mut io::sink());
            let _ = eof_tx.send(());
        });
        thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let _ = io::copy(&mut reader, &mut io::sink());
        });
        SessionOwnerHelperProcess {
            child,
            stdin: Some(stdin),
            ready_rx,
            eof_rx,
        }
    }

    fn wait_for_session_owner_ready(helper: &mut SessionOwnerHelperProcess) {
        match helper.ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => panic!("session owner helper handshake failed: {error}"),
            Err(error) => panic!("timed out waiting for session owner helper: {error}"),
        }
    }

    fn release_session_owner_helper(helper: &mut SessionOwnerHelperProcess) {
        let mut stdin = helper.stdin.take().expect("session owner helper stdin");
        writeln!(stdin, "release").expect("send session owner release");
        stdin.flush().expect("flush session owner release");
    }

    fn wait_for_session_owner_exit(helper: &mut SessionOwnerHelperProcess) {
        if helper.eof_rx.recv_timeout(Duration::from_secs(5)).is_err() {
            panic!("timed out waiting for session owner helper to exit");
        }
        let status = helper.child.wait().expect("wait for session owner helper");
        assert!(
            status.success(),
            "session owner helper exit status: {status}"
        );
    }

    fn wait_for_session_owner_slots(service: &SourceRegistryService) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if matches!(service.catalog_status(), CatalogStatus::Ready)
                && matches!(service.vocabulary_status(), VocabularyStatus::Available)
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for session owner slots: catalog={:?}, vocabulary={:?}",
                service.catalog_status(),
                service.vocabulary_status()
            );
            thread::yield_now();
        }
    }

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
            .join(".yield")
            .join(PROFILE_DIR_NAME)
            .join(GPUI_DEVELOPMENT_PROFILE);
        fs::create_dir_all(&root).expect("profile root");
        fs::canonicalize(root).expect("canonical profile root")
    }

    const PENDING_INTENT_RECORD: &[u8] = br#"{
  "schema_version": 3,
  "operation_id": "01234567-89ab-cdef-0123-456789abcdef",
  "intent": {"actor": "User", "kind": "FileMutation", "label": "session fixture"},
  "universal_intent": {
    "parent_operation_id": null,
    "command_kind": "CreateFile",
    "user_status_key": "operation.status.create-file",
    "participants": {
      "descriptors": [{
        "id": "fedcba98-7654-3210-fedc-ba9876543210",
        "kind": "GlobalLibrary",
        "requirement": "Required",
        "source_fence": null,
        "artifact_key": null
      }],
      "checkpoints": [{
        "participant_id": "fedcba98-7654-3210-fedc-ba9876543210",
        "state": "Pending",
        "committed_revision": null,
        "diagnostic_code": null
      }]
    }
  },
  "phase": "IntentDurable",
  "disposition": "None",
  "payload": {"source": "session fixture"},
  "capacity_plan": {
    "volumes": [{
      "identity": {"device": 1},
      "allocation_unit": 1,
      "allocation_class": "DestinationStaging",
      "logical_bytes": 1,
      "allocated_bytes": 1,
      "protected_free_bytes": 268435456
    }]
  },
  "created_unix_ms": 31,
  "updated_unix_ms": 32
}"#;

    fn journal_database_path(profile_root: &Path) -> PathBuf {
        profile_root
            .join(JOURNAL_DATABASE_ROOT_NAME)
            .join(JOURNAL_DATABASE_FILE_NAME)
    }

    fn seed_pending_journal(profile_root: &Path) -> OperationId {
        let app_root_guard = AppRootGuard::set(profile_root.to_path_buf()).expect("app root");
        let profile_guard =
            WritableProfileGuard::acquire_current().expect("fixture profile ownership");
        let journal = OperationJournalStore::open(&profile_guard).expect("fixture journal open");
        let record = decode(PENDING_INTENT_RECORD).expect("pending intent fixture decodes");
        let operation_id = record.operation_id;
        journal
            .insert_intent(&record)
            .expect("pending intent fixture persists");
        drop(journal);
        drop(profile_guard);
        drop(app_root_guard);
        operation_id
    }

    fn seed_full_progress_raw(
        profile_root: &Path,
        operation_id: &str,
        sequence: i64,
        record: &[u8],
    ) {
        let app_root_guard = AppRootGuard::set(profile_root.to_path_buf()).expect("app root");
        let profile_guard =
            WritableProfileGuard::acquire_current().expect("fixture profile ownership");
        let journal = OperationJournalStore::open(&profile_guard).expect("fixture journal open");
        drop(journal);
        drop(profile_guard);
        drop(app_root_guard);

        let connection =
            Connection::open(journal_database_path(profile_root)).expect("open fixture database");
        connection
            .execute(
                "INSERT INTO operation_journal_full_progress (operation_id, sequence, record)
                 VALUES (?1, ?2, ?3)",
                rusqlite::params![operation_id, sequence, record],
            )
            .expect("insert full-progress fixture row");
    }

    fn seed_full_progress_fixture(profile_root: &Path) {
        seed_full_progress_raw(
            profile_root,
            FULL_PROGRESS_FIXTURE_OPERATION_ID,
            1,
            FULL_PROGRESS_FIXTURE,
        );
    }

    fn write_test_wav(path: &Path) {
        let mut bytes = Vec::with_capacity(46);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(38_u32).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&(16_u32).to_le_bytes());
        bytes.extend_from_slice(&(1_u16).to_le_bytes());
        bytes.extend_from_slice(&(1_u16).to_le_bytes());
        bytes.extend_from_slice(&(8_000_u32).to_le_bytes());
        bytes.extend_from_slice(&(16_000_u32).to_le_bytes());
        bytes.extend_from_slice(&(2_u16).to_le_bytes());
        bytes.extend_from_slice(&(16_u16).to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(2_u32).to_le_bytes());
        bytes.extend_from_slice(&(0_i16).to_le_bytes());
        fs::write(path, bytes).expect("test WAV");
    }

    fn wait_for_event(
        service: &SourceRegistryService,
        predicate: impl Fn(&RegistryEvent) -> bool,
    ) -> RegistryEvent {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut observed = Vec::new();
        loop {
            match service.try_recv() {
                Ok(event) if predicate(&event) => return event,
                Ok(event) => observed.push(format!("{event:?}")),
                Err(TryRecvError::Empty) => {
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting for session event; observed: {observed:?}"
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
        let mut observed = Vec::new();
        loop {
            match service.try_recv_preference() {
                Ok(event) if predicate(&event) => return event,
                Ok(event) => observed.push(format!("{event:?}")),
                Err(TryRecvError::Empty) => {
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting for preference event; observed: {observed:?}"
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

    fn wait_for_maintenance<T>(request: &MaintenanceRequest<T>) -> Result<T, MaintenanceError> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match request.try_result() {
                Ok(result) => return result,
                Err(TryRecvError::Empty) => {
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting for maintenance result"
                    );
                    thread::sleep(Duration::from_millis(5));
                }
                Err(TryRecvError::Disconnected) => {
                    panic!("maintenance result channel disconnected")
                }
            }
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
        wait_for_catalog_event_with_budget(
            service,
            predicate,
            Duration::from_secs(60),
            "catalog event",
        )
    }

    fn wait_for_catalog_event_with_budget(
        service: &SourceRegistryService,
        predicate: impl Fn(&CatalogEvent) -> bool,
        budget: Duration,
        phase: &str,
    ) -> CatalogEvent {
        let deadline = Instant::now() + budget;
        let mut other_events = Vec::new();
        loop {
            match service.try_recv_catalog() {
                Ok(event) if predicate(&event) => return event,
                Ok(event) => {
                    if other_events.len() < 8 {
                        other_events.push(format!("{event:?}"));
                    }
                }
                Err(TryRecvError::Empty) => {
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting for {phase}; other events: {other_events:?}"
                    );
                    thread::sleep(Duration::from_millis(5));
                }
                Err(TryRecvError::Disconnected) => {
                    panic!("catalog event channel disconnected")
                }
            }
        }
    }

    fn wait_for_catalog_event_retaining_unmatched(
        service: &SourceRegistryService,
        pending: &mut Vec<CatalogEvent>,
        predicate: impl Fn(&CatalogEvent) -> bool,
        budget: Duration,
        phase: &str,
    ) -> CatalogEvent {
        if let Some(index) = pending.iter().position(&predicate) {
            return pending.remove(index);
        }
        let deadline = Instant::now() + budget;
        loop {
            match service.try_recv_catalog() {
                Ok(event) if predicate(&event) => return event,
                Ok(event) => {
                    assert!(
                        pending.len() < 32,
                        "too many catalog events while waiting for {phase}"
                    );
                    pending.push(event);
                }
                Err(TryRecvError::Empty) => {
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting for {phase}; pending events: {pending:?}"
                    );
                    thread::sleep(Duration::from_millis(5));
                }
                Err(TryRecvError::Disconnected) => {
                    panic!("catalog event channel disconnected while waiting for {phase}")
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
            CanonicalSourceRoot::new(std::env::temp_dir().join("..").join("source")),
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
            SourceRegistryService::start_portable_registry_for_test(profile_root.clone())
                .expect("service start");
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
            .join(".yield")
            .join(PROFILE_DIR_NAME)
            .join(GPUI_DEVELOPMENT_PROFILE)
            .join("library.db");
        assert!(development_db.is_file());
        assert!(!temp.path().join(".yield/library.db").exists());

        let mut reopened = SourceRegistryService::start_portable_registry_for_test(profile_root)
            .expect("reopen service");
        let restored = wait_for_snapshot(&reopened, |snapshot| {
            snapshot
                .as_slice()
                .iter()
                .any(|source| source.root == source_root.as_path())
        });
        assert_eq!(restored.len(), 1);
        reopened.shutdown().expect("reopen shutdown");
    }

    fn seed_primary_registry(temp: &TempDir) -> (PathBuf, Vec<SourceId>) {
        let profile = profile_root(temp);
        let mut service = SourceRegistryService::start_portable_registry_for_test(profile.clone())
            .expect("start fixture");
        wait_for_snapshot(&service, |_| true);
        for name in ["first", "second", "protected"] {
            let request = service.add_root(root(temp, name)).expect("add fixture");
            wait_for_event(&service, |event| {
                matches!(event,
                RegistryEvent::RequestApplied { request: actual, .. } if *actual == request)
            });
            wait_for_event(&service, |event| {
                matches!(event, RegistryEvent::Persisted { .. })
            });
        }
        service.shutdown().expect("seed shutdown");
        let connection = Connection::open(profile.join("library.db")).expect("fixture database");
        let ids = connection
            .prepare("SELECT id FROM sources ORDER BY sort_order")
            .expect("query")
            .query_map([], |row| row.get::<_, String>(0))
            .expect("ids")
            .map(|id| SourceId::from_string(id.expect("id")))
            .collect::<Vec<_>>();
        connection
            .execute(
                "UPDATE sources SET role = 'protected' WHERE id = ?1",
                [ids[2].as_str()],
            )
            .expect("protected fixture");
        (profile, ids)
    }

    fn persist_primary_edit(
        service: &SourceRegistryService,
        id: &SourceId,
        primary: bool,
        revision: RegistryRevision,
    ) -> RegistryRevision {
        let request = service
            .set_source_primary(id.clone(), primary, revision)
            .expect("submit role");
        let RegistryEvent::RequestApplied { revision, .. } = wait_for_event(
            service,
            |event| matches!(event, RegistryEvent::RequestApplied { request: actual, .. } if *actual == request),
        ) else {
            unreachable!()
        };
        wait_for_event(
            service,
            |event| matches!(event, RegistryEvent::Persisted { revision: actual } if *actual == revision),
        );
        revision
    }

    #[test]
    fn primary_source_persists_replacement_clear_and_rejects_stale_or_protected_ids() {
        let temp = TempDir::new().expect("fixture");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let (profile, ids) = seed_primary_registry(&temp);
        let mut service = SourceRegistryService::start_portable_registry_for_test(profile.clone())
            .expect("start");
        let before = wait_for_snapshot(&service, |_| true);
        let revision = persist_primary_edit(&service, &ids[0], true, RegistryRevision::default());
        for (id, expected) in [
            (&ids[1], RegistryRevision::default()),
            (&ids[2], revision),
            (&SourceId::from_string("removed-source"), revision),
        ] {
            let request = service
                .set_source_primary(id.clone(), true, expected)
                .expect("submit rejected role");
            wait_for_event(
                &service,
                |event| matches!(event, RegistryEvent::RequestFailed { request: actual, .. } if *actual == request),
            );
        }
        persist_primary_edit(&service, &ids[1], true, revision);
        service.shutdown().expect("shutdown");
        let mut reopened = SourceRegistryService::start_portable_registry_for_test(profile.clone())
            .expect("reopen");
        let restored = wait_for_snapshot(&reopened, |_| true);
        let mut expected = before.as_slice().to_vec();
        expected[1].role = SourceRole::Primary;
        assert!(
            sources_equal(restored.as_slice(), &expected),
            "only role changed; identity, order, roots and metadata survive"
        );
        persist_primary_edit(&reopened, &ids[1], false, RegistryRevision::default());
        reopened.shutdown().expect("clear shutdown");
        let mut reopened =
            SourceRegistryService::start_portable_registry_for_test(profile).expect("reopen clear");
        assert!(sources_equal(
            wait_for_snapshot(&reopened, |_| true).as_slice(),
            before.as_slice()
        ));
        assert!(
            !temp.path().join("second").exists(),
            "role edit creates no directories"
        );
        reopened.shutdown().expect("shutdown");
    }

    #[test]
    fn primary_source_removed_and_reregistered_path_rejects_old_revision() {
        let temp = TempDir::new().expect("fixture");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let (profile, ids) = seed_primary_registry(&temp);
        let mut service =
            SourceRegistryService::start_portable_registry_for_test(profile).expect("start");
        wait_for_snapshot(&service, |_| true);
        let request = service.remove_root(root(&temp, "first")).expect("remove");
        wait_for_event(
            &service,
            |event| matches!(event, RegistryEvent::RequestApplied { request: actual, .. } if *actual == request),
        );
        wait_for_event(&service, |event| {
            matches!(event, RegistryEvent::Persisted { .. })
        });
        let request = service.add_root(root(&temp, "first")).expect("replacement");
        let restored = wait_for_snapshot(&service, |snapshot| snapshot.len() == 3);
        let replacement = restored
            .as_slice()
            .iter()
            .find(|source| source.root == root(&temp, "first").as_path())
            .expect("replacement source");
        assert_eq!(
            replacement.id, ids[0],
            "retained root identity survives re-registration"
        );
        let RegistryEvent::RequestApplied { revision, .. } = wait_for_event(
            &service,
            |event| matches!(event, RegistryEvent::RequestApplied { request: actual, .. } if *actual == request),
        ) else {
            unreachable!()
        };
        wait_for_event(
            &service,
            |event| matches!(event, RegistryEvent::Persisted { revision: actual } if *actual == revision),
        );
        let request = service
            .set_source_primary(ids[0].clone(), true, RegistryRevision::default())
            .expect("old target");
        wait_for_event(
            &service,
            |event| matches!(event, RegistryEvent::RequestFailed { request: actual, .. } if *actual == request),
        );
        persist_primary_edit(&service, &replacement.id, true, revision);
        service.shutdown().expect("shutdown");
    }

    #[test]
    fn primary_source_save_failure_retains_prior_durable_roles() {
        let temp = TempDir::new().expect("fixture");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let (profile, ids) = seed_primary_registry(&temp);
        Connection::open(profile.join("library.db")).expect("database").execute_batch(
            "CREATE TRIGGER fail_primary_registry BEFORE DELETE ON sources BEGIN SELECT RAISE(FAIL, 'primary save fault'); END;"
        ).expect("inject save failure");
        let mut service = SourceRegistryService::start_portable_registry_for_test(profile.clone())
            .expect("start");
        let before = wait_for_snapshot(&service, |_| true);
        let _request = service
            .set_source_primary(ids[0].clone(), true, RegistryRevision::default())
            .expect("submit");
        let RegistryEvent::PersistenceUnavailable { error, .. } =
            wait_for_event(&service, |event| {
                matches!(event, RegistryEvent::PersistenceUnavailable { .. })
            })
        else {
            unreachable!()
        };
        assert!(error.to_string().contains("primary save fault"));
        service
            .shutdown()
            .expect_err("failed persistence remains unavailable");
        let mut reopened =
            SourceRegistryService::start_portable_registry_for_test(profile).expect("reopen");
        assert!(sources_equal(
            wait_for_snapshot(&reopened, |_| true).as_slice(),
            before.as_slice()
        ));
        reopened.shutdown().expect("shutdown without writes");
    }

    #[test]
    fn primary_source_ambiguous_identity_is_rejected_without_mutation() {
        let temp = TempDir::new().expect("fixture");
        let id = SourceId::from_string("duplicate");
        let mut sources = vec![
            source(&root(&temp, "a"), id.as_str()),
            source(&root(&temp, "b"), id.as_str()),
        ];
        let before = sources.clone();
        assert!(set_primary_source(&mut sources, &id, true).is_err());
        assert!(sources_equal(&sources, &before));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn normal_session_bootstrap_retains_unsupported_reserve_failure() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let mut service = SourceRegistryService::start_inner(Some(profile_root(&temp)))
            .expect("normal session starts worker");
        let event = wait_for_event(&service, |event| {
            matches!(event, RegistryEvent::PersistenceUnavailable { .. })
        });
        let RegistryEvent::PersistenceUnavailable { error, .. } = event else {
            unreachable!()
        };
        assert!(
            error
                .to_string()
                .contains("protected reserve namespace Unsupported")
        );
        let request = service
            .reorder_source(
                SourceId::from_string("missing-a"),
                SourceId::from_string("missing-b"),
                true,
                RegistryRevision::default(),
            )
            .expect("request enters bounded queue");
        wait_for_event(
            &service,
            |event| matches!(event, RegistryEvent::RequestFailed { request: failed, .. } if *failed == request),
        );
        let error = service
            .shutdown()
            .expect_err("shutdown retains the qualification failure");
        assert!(
            error
                .to_string()
                .contains("protected reserve namespace Unsupported")
        );
    }

    #[test]
    fn source_reorder_persists_identity_and_rejects_stale_revision() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile = profile_root(&temp);
        let mut service = SourceRegistryService::start_portable_registry_for_test(profile.clone())
            .expect("start");
        wait_for_event(&service, |event| {
            matches!(event, RegistryEvent::Snapshot { .. })
        });

        let roots = [
            root(&temp, "first"),
            root(&temp, "second"),
            root(&temp, "third"),
        ];
        let mut revision = RegistryRevision::default();
        let mut before = None;
        for (index, source_root) in roots.iter().enumerate() {
            let request = service.add_root(source_root.clone()).expect("add source");
            if index == roots.len() - 1 {
                before = Some(wait_for_snapshot(&service, |snapshot| {
                    snapshot.len() == roots.len()
                }));
            }
            let event = wait_for_event(
                &service,
                |event| matches!(event, RegistryEvent::RequestApplied { request: applied, .. } if *applied == request),
            );
            let RegistryEvent::RequestApplied {
                revision: applied, ..
            } = event
            else {
                unreachable!()
            };
            revision = applied;
        }
        let before = before.expect("complete source snapshot");
        let ids = before
            .as_slice()
            .iter()
            .map(|source| source.id.clone())
            .collect::<Vec<_>>();
        let request = service
            .reorder_source(ids[0].clone(), ids[2].clone(), true, revision)
            .expect("reorder");
        let reordered = wait_for_snapshot(&service, |snapshot| {
            snapshot
                .as_slice()
                .iter()
                .map(|source| &source.id)
                .collect::<Vec<_>>()
                == vec![&ids[1], &ids[2], &ids[0]]
        });
        assert_eq!(
            reordered
                .as_slice()
                .iter()
                .map(|source| source.root.as_path())
                .collect::<Vec<_>>(),
            vec![roots[1].as_path(), roots[2].as_path(), roots[0].as_path()]
        );
        assert!(sources_equal(
            reordered.as_slice(),
            &[
                before.as_slice()[1].clone(),
                before.as_slice()[2].clone(),
                before.as_slice()[0].clone(),
            ]
        ));
        let applied = wait_for_event(
            &service,
            |event| matches!(event, RegistryEvent::RequestApplied { request: actual, .. } if *actual == request),
        );
        let RegistryEvent::RequestApplied {
            revision: reordered_revision,
            ..
        } = applied
        else {
            unreachable!()
        };
        wait_for_event(
            &service,
            |event| matches!(event, RegistryEvent::Persisted { revision } if *revision == reordered_revision),
        );
        let stale = service
            .reorder_source(ids[0].clone(), ids[1].clone(), false, revision)
            .expect("submit stale reorder");
        wait_for_event(
            &service,
            |event| matches!(event, RegistryEvent::RequestFailed { request, .. } if *request == stale),
        );
        let missing = service
            .reorder_source(
                SourceId::from_string("missing-source"),
                ids[1].clone(),
                false,
                reordered_revision,
            )
            .expect("submit removed source reorder");
        wait_for_event(
            &service,
            |event| matches!(event, RegistryEvent::RequestFailed { request, .. } if *request == missing),
        );
        service.shutdown().expect("shutdown");

        let mut reopened =
            SourceRegistryService::start_portable_registry_for_test(profile).expect("reopen");
        let restored = wait_for_snapshot(&reopened, |snapshot| snapshot.len() == roots.len());
        assert_eq!(
            restored
                .as_slice()
                .iter()
                .map(|source| &source.id)
                .collect::<Vec<_>>(),
            vec![&ids[1], &ids[2], &ids[0]]
        );
        reopened.shutdown().expect("reopen shutdown");
    }

    #[test]
    fn session_journal_reopens_after_orderly_shutdown() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile_root = profile_root(&temp);
        let mut service =
            SourceRegistryService::start_inner(Some(profile_root.clone())).expect("service start");
        wait_for_session_owner_slots(&service);

        let journal_path = journal_database_path(&profile_root);
        assert!(
            journal_path.is_file(),
            "session should open its journal before writers"
        );
        service.shutdown().expect("session shutdown");

        let app_root_guard = AppRootGuard::set(profile_root.clone()).expect("app root");
        let profile_guard =
            WritableProfileGuard::acquire_current().expect("profile ownership after shutdown");
        let journal = OperationJournalStore::open(&profile_guard).expect("journal reopens");
        assert!(
            journal
                .get(OperationId::new())
                .expect("empty journal point read")
                .is_none()
        );
        drop(journal);
        drop(profile_guard);
        drop(app_root_guard);
    }

    #[test]
    fn persistence_drop_stops_external_catalog_before_releasing_journal() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile_root = profile_root(&temp);
        let catalog_slot = Arc::new(Mutex::new(CatalogSlot::loading()));
        let vocabulary_slot = Arc::new(Mutex::new(VocabularySlot::loading()));
        let maintenance_admission = Arc::new(MaintenanceAdmission::new());
        let (persistence, _root_guard, _profile_guard) = Persistence::start(
            Some(profile_root.clone()),
            Arc::clone(&catalog_slot),
            Arc::clone(&vocabulary_slot),
            maintenance_admission,
            false,
        );
        let external_catalog = persistence
            .catalog
            .as_ref()
            .expect("catalog started")
            .clone();
        drop(persistence);

        assert_eq!(
            external_catalog.submit(CatalogCommand::ReadSnapshot {
                source_id: SourceId::from_string("drop-order"),
                expected_revision: None,
            }),
            Err(yield_catalog::CatalogSubmitError::Closed)
        );
        drop(external_catalog);
        drop(catalog_slot);
        drop(vocabulary_slot);

        let profile_guard = WritableProfileGuard::acquire_current()
            .expect("profile released after persistence drop");
        let journal = OperationJournalStore::open(&profile_guard).expect("journal reopens");
        drop(journal);
        drop(profile_guard);
    }

    #[test]
    fn corrupt_operation_journal_blocks_writers_and_preserves_bytes() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile_root = profile_root(&temp);
        let journal_path = journal_database_path(&profile_root);
        fs::create_dir_all(journal_path.parent().expect("journal directory"))
            .expect("journal directory");
        let retained = b"corrupt operation journal bytes".to_vec();
        fs::write(&journal_path, &retained).expect("corrupt journal fixture");

        let mut service =
            SourceRegistryService::start_inner(Some(profile_root.clone())).expect("service start");
        wait_for_event(&service, |event| {
            matches!(
                event,
                RegistryEvent::PersistenceUnavailable {
                    error: SessionError::Persistence(reason),
                    ..
                } if reason.contains("operation journal unavailable")
            )
        });
        assert!(matches!(
            service.catalog_status(),
            CatalogStatus::Unavailable { ref reason }
                if reason.contains("operation journal unavailable")
        ));
        assert!(matches!(
            service.lookup_profile_tag_categories(vec![String::from("journal-blocked")]),
            Err(VocabularySubmitError::Unavailable(reason))
                if reason.contains("operation journal unavailable")
        ));
        assert!(!profile_root.join("library.db").exists());
        assert_eq!(
            fs::read(&journal_path).expect("retained journal bytes"),
            retained
        );
        assert!(matches!(
            service.shutdown(),
            Err(ShutdownError::Session(SessionError::Persistence(reason)))
                if reason.contains("operation journal unavailable")
        ));
    }

    #[test]
    fn pending_operation_journal_blocks_ordinary_writers_and_remains_reopenable() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile_root = profile_root(&temp);
        let operation_id = seed_pending_journal(&profile_root);
        let journal_path = journal_database_path(&profile_root);
        let retained_database = fs::read(&journal_path).expect("journal bytes");

        let mut service =
            SourceRegistryService::start_inner(Some(profile_root.clone())).expect("service start");
        wait_for_event(&service, |event| {
            matches!(
                event,
                RegistryEvent::PersistenceUnavailable {
                    error: SessionError::RecoveryUnavailable(reason),
                    ..
                } if reason.contains("require recovery")
            )
        });
        assert!(matches!(
            service.catalog_status(),
            CatalogStatus::Unavailable { ref reason }
                if reason.contains("recovery unavailable")
        ));
        assert!(matches!(
            service.lookup_profile_tag_categories(vec![String::from("recovery-blocked")]),
            Err(VocabularySubmitError::Unavailable(reason))
                if reason.contains("recovery unavailable")
        ));
        assert!(!profile_root.join("library.db").exists());
        assert_eq!(
            fs::read(&journal_path).expect("journal remains retained"),
            retained_database
        );
        assert!(matches!(
            service.shutdown(),
            Err(ShutdownError::Session(SessionError::RecoveryUnavailable(reason)))
                if reason.contains("require recovery")
        ));

        let app_root_guard = AppRootGuard::set(profile_root.clone()).expect("app root");
        let profile_guard =
            WritableProfileGuard::acquire_current().expect("profile ownership after blocked start");
        let journal = OperationJournalStore::open(&profile_guard).expect("journal reopens");
        assert!(
            journal
                .get(operation_id)
                .expect("retained intent point read")
                .is_some()
        );
        drop(journal);
        drop(profile_guard);
        drop(app_root_guard);
    }

    #[test]
    fn full_progress_only_operation_journal_blocks_ordinary_writers() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile_root = profile_root(&temp);
        seed_full_progress_fixture(&profile_root);
        let journal_path = journal_database_path(&profile_root);
        let retained_database = fs::read(&journal_path).expect("journal bytes");

        let mut service =
            SourceRegistryService::start_inner(Some(profile_root.clone())).expect("service start");
        wait_for_event(&service, |event| {
            matches!(
                event,
                RegistryEvent::PersistenceUnavailable {
                    error: SessionError::RecoveryUnavailable(reason),
                    ..
                } if reason.contains("full-progress")
                    && reason.contains("consistent observation requires coordinator recovery")
            )
        });
        assert!(!profile_root.join("library.db").exists());
        assert_eq!(
            fs::read(&journal_path).expect("retained journal bytes"),
            retained_database
        );
        assert!(matches!(
            service.shutdown(),
            Err(ShutdownError::Session(SessionError::RecoveryUnavailable(reason)))
                if reason.contains("full-progress")
        ));
    }

    #[test]
    fn malformed_full_progress_operation_journal_blocks_and_preserves_bytes() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile_root = profile_root(&temp);
        seed_full_progress_raw(
            &profile_root,
            FULL_PROGRESS_FIXTURE_OPERATION_ID,
            1,
            b"not-json",
        );
        let journal_path = journal_database_path(&profile_root);
        let retained_database = fs::read(&journal_path).expect("journal bytes");

        let mut service =
            SourceRegistryService::start_inner(Some(profile_root.clone())).expect("service start");
        wait_for_event(&service, |event| {
            matches!(
                event,
                RegistryEvent::PersistenceUnavailable {
                    error: SessionError::RecoveryUnavailable(reason),
                    ..
                } if reason.contains("full-progress")
                    && reason.contains("retained audit evidence")
                    && reason.contains("malformed")
            )
        });
        assert!(!profile_root.join("library.db").exists());
        assert_eq!(
            fs::read(&journal_path).expect("retained journal bytes"),
            retained_database
        );
        assert!(matches!(
            service.shutdown(),
            Err(ShutdownError::Session(SessionError::RecoveryUnavailable(reason)))
                if reason.contains("malformed")
        ));
    }

    #[test]
    fn mixed_legacy_and_full_progress_ambiguity_blocks_and_preserves_rows() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile_root = profile_root(&temp);
        let operation_id = seed_pending_journal(&profile_root);
        seed_full_progress_raw(
            &profile_root,
            &operation_id.to_string(),
            2,
            FULL_PROGRESS_FIXTURE,
        );
        let journal_path = journal_database_path(&profile_root);
        let retained_database = fs::read(&journal_path).expect("journal bytes");

        let mut service =
            SourceRegistryService::start_inner(Some(profile_root.clone())).expect("service start");
        wait_for_event(&service, |event| {
            matches!(
                event,
                RegistryEvent::PersistenceUnavailable {
                    error: SessionError::RecoveryUnavailable(reason),
                    ..
                } if reason.contains("both journal families")
            )
        });
        assert!(!profile_root.join("library.db").exists());
        assert_eq!(
            fs::read(&journal_path).expect("retained journal bytes"),
            retained_database
        );
        assert!(matches!(
            service.shutdown(),
            Err(ShutdownError::Session(SessionError::RecoveryUnavailable(reason)))
                if reason.contains("both journal families")
        ));
    }

    #[test]
    fn vocabulary_service_reports_initializing_before_owner_start() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let gate = Arc::new(std::sync::Barrier::new(2));
        let mut service = SourceRegistryService::start_portable_vocabulary_with_gate_for_test(
            profile_root(&temp),
            Arc::clone(&gate),
        )
        .expect("service start");

        let status = service.vocabulary_status();
        let request = service.lookup_profile_tag_categories(vec![String::from("kick")]);
        let deletion = service.delete_profile_tag_vocabulary(0, String::from("kick"));
        // Release the worker before assertions so a failed assertion can still shut it down.
        gate.wait();
        assert_eq!(status, VocabularyStatus::Initializing);
        assert!(matches!(request, Err(VocabularySubmitError::Initializing)));
        assert!(matches!(deletion, Err(VocabularySubmitError::Initializing)));
        wait_for_vocabulary_available(&service);
        service.shutdown().expect("shutdown");
    }

    #[test]
    fn maintenance_admission_serializes_and_blocks_protected_admissions() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let gate = Arc::new(std::sync::Barrier::new(2));
        let mut service = SourceRegistryService::start_inner_with_initialization_gate_for_test(
            Some(profile_root(&temp)),
            Arc::clone(&gate),
        )
        .expect("service start");

        let request = service
            .clear_rebuildable_cache()
            .expect("cache maintenance admission");
        assert!(matches!(
            service.request_storage_usage(),
            Err(SubmitError::MaintenanceActive)
        ));
        assert!(matches!(
            service.add_root(root(&temp, "blocked-source")),
            Err(SubmitError::MaintenanceActive)
        ));
        assert!(matches!(
            service.patch_preferences(PreferencePatch::SetTrashFolder(Some(
                temp.path().join("trash"),
            ))),
            Err(SubmitError::MaintenanceActive)
        ));

        gate.wait();
        assert!(matches!(
            wait_for_maintenance(&request),
            Err(MaintenanceError::NotReady)
        ));
        drop(request);
        service.shutdown().expect("shutdown");
    }

    #[test]
    fn loaded_cache_clear_rejects_source_and_trash_namespace_conflicts() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile = profile_root(&temp);
        let cache = profile.join("cache");
        fs::create_dir_all(&cache).expect("cache root");
        let mut service = SourceRegistryService::start_inner(Some(profile)).expect("service start");
        wait_for_snapshot(&service, |snapshot| snapshot.as_slice().is_empty());
        wait_for_preference_snapshot(&service, |_| true);

        let cache_root = CanonicalSourceRoot::new(cache.clone()).expect("cache source root");
        let add_request = service
            .add_root(cache_root.clone())
            .expect("add cache source");
        wait_for_snapshot(&service, |snapshot| {
            snapshot
                .as_slice()
                .iter()
                .any(|source| source.root == cache)
        });
        wait_for_event(&service, |event| {
            matches!(
                event,
                RegistryEvent::RequestApplied { request, .. } if *request == add_request
            )
        });

        let source_conflict = service
            .clear_rebuildable_cache()
            .expect("source-conflict admission");
        assert!(matches!(
            wait_for_maintenance(&source_conflict),
            Err(MaintenanceError::SourceRootConflict { path }) if path == cache
        ));

        let remove_request = service
            .remove_root(cache_root)
            .expect("remove cache source");
        wait_for_event(&service, |event| {
            matches!(
                event,
                RegistryEvent::RequestApplied { request, .. } if *request == remove_request
            )
        });
        let trash = cache.join("trash");
        let trash_request = service
            .patch_preferences(PreferencePatch::SetTrashFolder(Some(trash.clone())))
            .expect("set cache trash folder");
        wait_for_preference_event(&service, |event| {
            matches!(
                event,
                PreferenceEvent::RequestApplied { request, .. } if *request == trash_request
            )
        });

        let trash_conflict = service
            .clear_rebuildable_cache()
            .expect("trash-conflict admission");
        assert!(matches!(
            wait_for_maintenance(&trash_conflict),
            Err(MaintenanceError::TrashFolderConflict { path }) if path == trash
        ));

        let clear_trash_request = service
            .patch_preferences(PreferencePatch::SetTrashFolder(None))
            .expect("clear trash folder");
        wait_for_preference_event(&service, |event| {
            matches!(
                event,
                PreferenceEvent::RequestApplied { request, .. } if *request == clear_trash_request
            )
        });
        service.shutdown().expect("shutdown");
    }

    #[test]
    fn loaded_cache_clear_succeeds_and_releases_admission_for_storage_query() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile = profile_root(&temp);
        let cache = profile.join("cache");
        fs::create_dir_all(cache.join("nested")).expect("cache root");
        fs::write(cache.join("nested/payload.cache"), [0_u8; 47]).expect("cache payload");
        let mut service = SourceRegistryService::start_inner(Some(profile)).expect("service start");
        wait_for_snapshot(&service, |snapshot| snapshot.as_slice().is_empty());
        wait_for_preference_snapshot(&service, |_| true);

        let clear = service.clear_rebuildable_cache().expect("cache admission");
        let outcome = wait_for_maintenance(&clear).expect("cache clear");
        assert_eq!(outcome.removed_files, 1);
        assert_eq!(outcome.removed_bytes, 47);
        assert_eq!(outcome.remaining_entries, 0);
        assert!(outcome.remaining_entries_known);
        assert!(!outcome.incomplete);
        assert!(cache.is_dir());
        assert!(!cache.join("nested").exists());

        let usage = service
            .request_storage_usage()
            .expect("storage admission after clear");
        let usage = wait_for_maintenance(&usage).expect("storage usage");
        assert_eq!(usage.cache_bytes, 0);
        service.shutdown().expect("shutdown");
    }

    #[test]
    fn dropping_admitted_cache_clear_is_joined_during_shutdown() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile = profile_root(&temp);
        let cache = profile.join("cache");
        fs::create_dir_all(&cache).expect("cache root");
        for index in 0..2_048_u32 {
            fs::write(cache.join(format!("payload-{index}.cache")), [0_u8; 8])
                .expect("cache payload");
        }
        let mut service = SourceRegistryService::start_inner(Some(profile)).expect("service start");
        wait_for_snapshot(&service, |snapshot| snapshot.as_slice().is_empty());
        wait_for_preference_snapshot(&service, |_| true);

        let request = service.clear_rebuildable_cache().expect("cache admission");
        thread::sleep(Duration::from_millis(1));
        drop(request);
        service
            .shutdown()
            .expect("shutdown joins cancelled maintenance");
    }

    #[test]
    fn vocabulary_service_rejects_old_acquisition_redirected_through_fresh_client() {
        let temp = TempDir::new().unwrap();
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).unwrap());
        let root = profile_root(&temp);
        let mut service =
            SourceRegistryService::start_portable_registry_for_test(root.clone()).unwrap();
        wait_for_vocabulary_available(&service);
        let original = service.vocabulary_acquisition_id().unwrap();
        let mut set = service
            .set_profile_tag_category_for_acquisition(
                original.clone(),
                0,
                "keep".into(),
                ProfileTagCategory::Character,
            )
            .unwrap();
        assert_eq!(set.acquisition_id(), &original);
        assert_eq!(
            set.mutation_context().unwrap().correlation(),
            set.id().get()
        );
        assert!(receive_vocabulary(&mut set).unwrap().changed);
        service.shutdown().unwrap();
        let mut reopened = SourceRegistryService::start_portable_registry_for_test(root).unwrap();
        wait_for_vocabulary_available(&reopened);
        assert_ne!(reopened.vocabulary_acquisition_id().unwrap(), original);
        let mut delayed_set = reopened
            .set_profile_tag_category_for_acquisition(
                original.clone(),
                1,
                "ghost".into(),
                ProfileTagCategory::Prefix,
            )
            .unwrap();
        let mut delayed_delete = reopened
            .delete_profile_tag_vocabulary_for_acquisition(original, 1, "keep".into())
            .unwrap();
        assert!(matches!(
            receive_vocabulary(&mut delayed_set),
            Err(LibraryError::GlobalMutationStaleAcquisition)
        ));
        assert!(matches!(
            receive_vocabulary(&mut delayed_delete),
            Err(LibraryError::GlobalMutationStaleAcquisition)
        ));
        let mut lookup = reopened
            .lookup_profile_tag_categories(vec!["keep".into(), "ghost".into()])
            .unwrap();
        let state = receive_vocabulary(&mut lookup).unwrap();
        assert_eq!(state.revision, 1);
        assert_eq!(state.entries.len(), 1);
        assert_eq!(state.entries[0].label, "keep");
        reopened.shutdown().unwrap();
    }

    #[test]
    fn vocabulary_service_lost_mutation_response_refreshes_without_recovering_receipt() {
        let temp = TempDir::new().unwrap();
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).unwrap());
        let root = profile_root(&temp);
        let mut service =
            SourceRegistryService::start_portable_registry_for_test(root.clone()).unwrap();
        wait_for_vocabulary_available(&service);
        let set = service
            .set_profile_tag_category(0, "keep".into(), ProfileTagCategory::Character)
            .unwrap();
        drop(set); // accepted changed result is never received
        service.shutdown().unwrap();
        let mut reopened =
            SourceRegistryService::start_portable_registry_for_test(root.clone()).unwrap();
        wait_for_vocabulary_available(&reopened);
        let mut lookup = reopened
            .lookup_profile_tag_categories(vec!["keep".into()])
            .unwrap();
        let refreshed = receive_vocabulary(&mut lookup).unwrap();
        assert_eq!(refreshed.entries.len(), 1);
        let mut current_noop = reopened
            .set_profile_tag_category(
                refreshed.revision,
                "keep".into(),
                ProfileTagCategory::Character,
            )
            .unwrap();
        assert!(!receive_vocabulary(&mut current_noop).unwrap().changed);
        let deletion = reopened
            .delete_profile_tag_vocabulary(refreshed.revision, "keep".into())
            .unwrap();
        drop(deletion); // accepted tombstone is likewise not received or durable Undo
        reopened.shutdown().unwrap();
        let mut final_service =
            SourceRegistryService::start_portable_registry_for_test(root).unwrap();
        wait_for_vocabulary_available(&final_service);
        let mut lookup = final_service
            .lookup_profile_tag_categories(vec!["keep".into()])
            .unwrap();
        let refreshed = receive_vocabulary(&mut lookup).unwrap();
        assert!(refreshed.entries.is_empty());
        let mut missing = final_service
            .delete_profile_tag_vocabulary(refreshed.revision, "keep".into())
            .unwrap();
        let current = receive_vocabulary(&mut missing).unwrap();
        assert_eq!(current.revision, refreshed.revision);
        assert!(
            current.removed.is_none(),
            "current no-op cannot recover the lost tombstone"
        );
        final_service.shutdown().unwrap();
    }

    #[test]
    fn vocabulary_service_mismatched_completion_releases_permit_without_receipt() {
        let temp = TempDir::new().unwrap();
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).unwrap());
        let mut service =
            SourceRegistryService::start_portable_registry_for_test(profile_root(&temp)).unwrap();
        wait_for_vocabulary_available(&service);
        let mut first = service
            .set_profile_tag_category(0, "keep".into(), ProfileTagCategory::Character)
            .unwrap();
        let mut other = service
            .set_profile_tag_category(0, "other".into(), ProfileTagCategory::Prefix)
            .unwrap();
        first.inject_wrong_context_for_test(other.mutation_context().unwrap().clone());
        assert!(matches!(
            receive_vocabulary(&mut first),
            Err(LibraryError::GlobalMutationCompletionMismatch)
        ));
        assert!(matches!(
            receive_vocabulary(&mut other),
            Err(LibraryError::ProfileTagVocabularyStaleRevision { .. })
        ));
        let mut lookup = service
            .lookup_profile_tag_categories(vec!["keep".into()])
            .unwrap();
        assert_eq!(receive_vocabulary(&mut lookup).unwrap().entries.len(), 1);

        let handles = (0..MAX_VOCABULARY_REQUESTS_IN_FLIGHT)
            .map(|_| {
                service
                    .lookup_profile_tag_categories(vec!["keep".into()])
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            service.lookup_profile_tag_categories(vec!["keep".into()]),
            Err(VocabularySubmitError::Full)
        ));
        drop(handles);
        service.shutdown().unwrap();
    }

    #[test]
    fn vocabulary_service_forwards_real_owner_requests_and_reopens() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile_root = profile_root(&temp);
        let mut service =
            SourceRegistryService::start_portable_registry_for_test(profile_root.clone())
                .expect("service start");
        wait_for_vocabulary_available(&service);

        let mut initial = service
            .lookup_profile_tag_categories(vec![String::from("kick"), String::from("kick")])
            .expect("initial lookup admission");
        assert_eq!(initial.id().get(), 1);
        let initial = receive_vocabulary(&mut initial).expect("initial lookup");
        assert_eq!(initial.revision, 0);
        assert!(initial.entries.is_empty());

        let mut set = service
            .set_profile_tag_category(0, String::from("kick"), ProfileTagCategory::SoundType)
            .expect("set admission");
        let receipt = receive_vocabulary(&mut set).expect("set result");
        assert_eq!(receipt.revision, 1);
        assert!(receipt.changed);

        let mut lookup = service
            .lookup_profile_tag_categories(vec![String::from("kick")])
            .expect("lookup admission");
        let lookup = receive_vocabulary(&mut lookup).expect("lookup result");
        assert_eq!(lookup.revision, 1);
        assert_eq!(lookup.entries.len(), 1);
        assert_eq!(lookup.entries[0].label, "kick");
        assert_eq!(lookup.entries[0].category, ProfileTagCategory::SoundType);

        let mut page = service
            .profile_tag_vocabulary_page(Some(1), String::from("k"), None, 64)
            .expect("page admission");
        let page = receive_vocabulary(&mut page).expect("page result");
        assert_eq!(page.revision, 1);
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].label, "kick");

        let mut stale = service
            .set_profile_tag_category(0, String::from("snare"), ProfileTagCategory::Character)
            .expect("stale admission");
        assert!(matches!(
            receive_vocabulary(&mut stale),
            Err(LibraryError::ProfileTagVocabularyStaleRevision {
                expected: 0,
                actual: 1
            })
        ));

        let oversized =
            "x".repeat(yield_library::sample_sources::PROFILE_TAG_VOCABULARY_MAX_LABEL_BYTES + 1);
        assert!(matches!(
            service.set_profile_tag_category(1, oversized, ProfileTagCategory::Character),
            Err(VocabularySubmitError::Invalid(
                ProfileTagVocabularyInputError::LabelTooLong { .. }
            ))
        ));

        service.shutdown().expect("shutdown");
        let mut reopened = SourceRegistryService::start_portable_registry_for_test(profile_root)
            .expect("reopen service");
        wait_for_vocabulary_available(&reopened);
        let mut restored = reopened
            .lookup_profile_tag_categories(vec![String::from("kick")])
            .expect("restored lookup admission");
        let restored = receive_vocabulary(&mut restored).expect("restored lookup");
        assert_eq!(restored.revision, 1);
        assert_eq!(restored.entries[0].label, "kick");
        reopened.shutdown().expect("reopen shutdown");
    }

    #[test]
    fn vocabulary_service_deletes_profile_entry_with_before_image_and_reopens() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile_root = profile_root(&temp);
        let mut service =
            SourceRegistryService::start_portable_registry_for_test(profile_root.clone())
                .expect("service start");
        wait_for_vocabulary_available(&service);

        let mut set = service
            .set_profile_tag_category(0, String::from("restore-me"), ProfileTagCategory::Prefix)
            .expect("set admission");
        let set = receive_vocabulary(&mut set).expect("set result");
        assert_eq!(set.revision, 1);

        let mut delete = service
            .delete_profile_tag_vocabulary(1, String::from("restore-me"))
            .expect("delete admission");
        let delete = receive_vocabulary(&mut delete).expect("delete result");
        assert_eq!(delete.revision, 2);
        assert_eq!(
            delete.removed,
            Some(ProfileTagVocabularyEntry {
                label: String::from("restore-me"),
                category: ProfileTagCategory::Prefix,
            })
        );

        let mut missing = service
            .delete_profile_tag_vocabulary(2, String::from("restore-me"))
            .expect("missing delete admission");
        let missing = receive_vocabulary(&mut missing).expect("missing delete result");
        assert_eq!(missing.revision, 2);
        assert!(missing.removed.is_none());

        let mut stale = service
            .delete_profile_tag_vocabulary(1, String::from("restore-me"))
            .expect("stale delete admission");
        assert!(matches!(
            receive_vocabulary(&mut stale),
            Err(LibraryError::ProfileTagVocabularyStaleRevision {
                expected: 1,
                actual: 2
            })
        ));

        let mut restore = service
            .set_profile_tag_category(2, String::from("restore-me"), ProfileTagCategory::Prefix)
            .expect("restore admission");
        let restore = receive_vocabulary(&mut restore).expect("restore result");
        assert_eq!(restore.revision, 3);

        service.shutdown().expect("shutdown");
        let mut reopened = SourceRegistryService::start_portable_registry_for_test(profile_root)
            .expect("reopen service");
        wait_for_vocabulary_available(&reopened);
        let mut lookup = reopened
            .lookup_profile_tag_categories(vec![String::from("restore-me")])
            .expect("lookup admission");
        let lookup = receive_vocabulary(&mut lookup).expect("lookup result");
        assert_eq!(lookup.revision, 3);
        assert_eq!(
            lookup.entries,
            vec![ProfileTagVocabularyEntry {
                label: String::from("restore-me"),
                category: ProfileTagCategory::Prefix,
            }]
        );
        reopened.shutdown().expect("reopen shutdown");
    }

    #[test]
    fn vocabulary_service_bounds_unconsumed_handles_and_releases_permits() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let mut service =
            SourceRegistryService::start_portable_registry_for_test(profile_root(&temp))
                .expect("service start");
        wait_for_vocabulary_available(&service);

        assert!(matches!(
            service.lookup_profile_tag_categories(vec![String::new()]),
            Err(VocabularySubmitError::Invalid(
                ProfileTagVocabularyInputError::EmptyLabel
            ))
        ));
        for label in ["loop", "oneshot", "one_shot", "one-shot"] {
            assert!(matches!(
                service.delete_profile_tag_vocabulary(0, String::from(label)),
                Err(VocabularySubmitError::Invalid(
                    ProfileTagVocabularyInputError::LockedPlaybackLabelCannotBeDeleted
                ))
            ));
        }

        let mut handles = Vec::new();
        for index in 0..MAX_VOCABULARY_REQUESTS_IN_FLIGHT {
            handles.push(
                service
                    .lookup_profile_tag_categories(vec![format!("unconsumed-{index}")])
                    .expect("bounded lookup admission"),
            );
        }
        assert!(matches!(
            service.lookup_profile_tag_categories(vec![String::from("blocked")]),
            Err(VocabularySubmitError::Full)
        ));

        receive_vocabulary(&mut handles[0]).expect("received permit result");
        handles.remove(0);
        let mut after_receive = service
            .lookup_profile_tag_categories(vec![String::from("after-receive")])
            .expect("admission after receive");
        receive_vocabulary(&mut after_receive).expect("after receive result");

        drop(handles.pop().expect("handle to drop"));
        let mut after_drop = service
            .lookup_profile_tag_categories(vec![String::from("after-drop")])
            .expect("admission after drop");
        receive_vocabulary(&mut after_drop).expect("after drop result");

        for mut handle in handles {
            receive_vocabulary(&mut handle).expect("remaining lookup result");
        }
        service.shutdown().expect("shutdown");
    }

    #[test]
    fn vocabulary_service_shutdown_drains_admitted_and_dropped_writes() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile_root = profile_root(&temp);
        let mut service =
            SourceRegistryService::start_portable_registry_for_test(profile_root.clone())
                .expect("service start");
        wait_for_vocabulary_available(&service);

        let mut retained = service
            .set_profile_tag_category(0, String::from("retained"), ProfileTagCategory::Prefix)
            .expect("retained write admission");
        service.shutdown().expect("shutdown");
        let retained = receive_vocabulary(&mut retained).expect("retained write result");
        assert_eq!(retained.revision, 1);
        assert!(matches!(
            service.lookup_profile_tag_categories(vec![String::from("retained")]),
            Err(VocabularySubmitError::Closed)
        ));

        let mut reopened =
            SourceRegistryService::start_portable_registry_for_test(profile_root.clone())
                .expect("reopen service");
        wait_for_vocabulary_available(&reopened);
        let dropped = reopened
            .set_profile_tag_category(1, String::from("dropped"), ProfileTagCategory::Character)
            .expect("dropped write admission");
        drop(dropped);
        reopened.shutdown().expect("second shutdown");

        let mut final_service =
            SourceRegistryService::start_portable_registry_for_test(profile_root)
                .expect("final service start");
        wait_for_vocabulary_available(&final_service);
        let mut restored = final_service
            .lookup_profile_tag_categories(vec![String::from("retained"), String::from("dropped")])
            .expect("final lookup admission");
        let restored = receive_vocabulary(&mut restored).expect("final lookup result");
        assert_eq!(restored.revision, 2);
        assert_eq!(restored.entries.len(), 2);
        final_service.shutdown().expect("final shutdown");
    }

    #[test]
    fn vocabulary_service_shutdown_drains_dropped_delete() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile_root = profile_root(&temp);
        let mut service =
            SourceRegistryService::start_portable_registry_for_test(profile_root.clone())
                .expect("service start");
        wait_for_vocabulary_available(&service);

        let mut set = service
            .set_profile_tag_category(
                0,
                String::from("drop-delete"),
                ProfileTagCategory::Character,
            )
            .expect("set admission");
        let set = receive_vocabulary(&mut set).expect("set result");
        assert_eq!(set.revision, 1);
        let delete = service
            .delete_profile_tag_vocabulary(1, String::from("drop-delete"))
            .expect("delete admission");
        drop(delete);
        service.shutdown().expect("shutdown");

        let mut reopened = SourceRegistryService::start_portable_registry_for_test(profile_root)
            .expect("reopen service");
        wait_for_vocabulary_available(&reopened);
        let mut lookup = reopened
            .lookup_profile_tag_categories(vec![String::from("drop-delete")])
            .expect("lookup admission");
        let lookup = receive_vocabulary(&mut lookup).expect("lookup result");
        assert_eq!(lookup.revision, 2);
        assert!(lookup.entries.is_empty());
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
        let mut pending_catalog_events = Vec::new();
        wait_for_catalog_event_retaining_unmatched(
            &service,
            &mut pending_catalog_events,
            |event| matches!(event, CatalogEvent::Registered { source_id: registered, .. } if registered == &source_id),
            Duration::from_secs(120),
            "fixture source registration",
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
        wait_for_catalog_event_retaining_unmatched(
            &service,
            &mut pending_catalog_events,
            |event| matches!(event, CatalogEvent::AuditCommitted { snapshot } if snapshot.source_id == source_id),
            Duration::from_secs(120),
            "256-folder fixture audit commit",
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
        match service.try_recv_catalog() {
            Ok(CatalogEvent::Closed) => assert!(matches!(
                service.try_recv_catalog(),
                Err(TryRecvError::Disconnected)
            )),
            Err(TryRecvError::Disconnected) => {}
            other => panic!("unexpected catalog shutdown event: {other:?}"),
        }
    }

    #[test]
    fn shutdown_retains_catalog_curation_receipt_and_reopens_persisted_revision() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile_root = profile_root(&temp);
        let source_root_path = temp.path().join("curation-source");
        fs::create_dir_all(&source_root_path).expect("source root");
        write_test_wav(&source_root_path.join("one.wav"));
        let source_root = fs::canonicalize(source_root_path).expect("canonical source root");
        let source_id = SourceId::from_string("session-curation-shutdown");
        let mut service =
            SourceRegistryService::start_inner(Some(profile_root.clone())).expect("service start");
        wait_for_catalog_ready(&service);
        service
            .submit_catalog(CatalogCommand::Register {
                source_id: source_id.clone(),
                canonical_root: source_root.clone(),
                lifecycle_generation: 1,
            })
            .expect("catalog registration");
        wait_for_catalog_event(&service, |event| {
            matches!(
                event,
                CatalogEvent::Registered {
                    source_id: registered,
                    lifecycle_generation: 1,
                } if registered == &source_id
            )
        });
        service
            .submit_catalog(CatalogCommand::RequestAudit {
                source_id: source_id.clone(),
                lifecycle_generation: 1,
            })
            .expect("catalog audit");
        let snapshot = match wait_for_catalog_event(&service, |event| {
            matches!(event, CatalogEvent::AuditCommitted { .. })
        }) {
            CatalogEvent::AuditCommitted { snapshot } => snapshot,
            _ => unreachable!(),
        };
        let curation_path =
            yield_library::sample_sources::reconciliation::RootRelativePath::try_from_path(
                PathBuf::from("one.wav"),
            )
            .expect("curation path");
        service
            .submit_catalog(CatalogCommand::ApplyCuration {
                source_id: source_id.clone(),
                lifecycle_generation: 1,
                expected_revision: snapshot.committed_revision,
                root_identity: snapshot.root_identity.clone(),
                request_id: 9001,
                updates: vec![SourceCurationUpdate::SetRating {
                    path: curation_path,
                    rating: yield_library::sample_sources::Rating::KEEP_1,
                }],
            })
            .expect("curation admission");
        service.shutdown().expect("immediate session shutdown");

        let committed_revision = match service
            .try_recv_catalog()
            .expect("retained curation receipt")
        {
            CatalogEvent::CurationCommitted {
                source_id: committed_source,
                lifecycle_generation: 1,
                request_id: 9001,
                receipt,
                ..
            } => {
                assert_eq!(committed_source, source_id);
                assert_eq!(receipt.files().len(), 1);
                assert_eq!(receipt.files()[0].rating.val(), 1);
                receipt.source_revision()
            }
            event => panic!("unexpected retained catalog event: {event:?}"),
        };
        assert!(matches!(
            service.try_recv_catalog(),
            Ok(CatalogEvent::Closed)
        ));
        assert!(matches!(
            service.try_recv_catalog(),
            Err(TryRecvError::Disconnected)
        ));
        drop(service);

        let mut reopened =
            SourceRegistryService::start_inner(Some(profile_root)).expect("reopen service");
        wait_for_catalog_ready(&reopened);
        reopened
            .submit_catalog(CatalogCommand::Register {
                source_id: source_id.clone(),
                canonical_root: source_root,
                lifecycle_generation: 2,
            })
            .expect("reopen catalog registration");
        wait_for_catalog_event(&reopened, |event| {
            matches!(
                event,
                CatalogEvent::Registered {
                    source_id: registered,
                    lifecycle_generation: 2,
                } if registered == &source_id
            )
        });
        let reopened_snapshot = match wait_for_catalog_event(&reopened, |event| {
            matches!(event, CatalogEvent::SnapshotReady { .. })
        }) {
            CatalogEvent::SnapshotReady { snapshot } => snapshot,
            _ => unreachable!(),
        };
        assert_eq!(reopened_snapshot.committed_revision, committed_revision);
        assert_eq!(
            reopened_snapshot
                .curation_metadata
                .get(Path::new("one.wav"))
                .expect("reopened curation metadata")
                .rating
                .val(),
            1
        );
        reopened.shutdown().expect("reopened session shutdown");
    }

    #[test]
    fn session_retains_compound_catalog_outcome_after_catalog_shutdown_admission_closes() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile_root = profile_root(&temp);
        let source_root_path = temp.path().join("compound-catalog-source");
        fs::create_dir_all(&source_root_path).expect("source root");
        write_test_wav(&source_root_path.join("one.wav"));
        let source_root = fs::canonicalize(source_root_path).expect("canonical source root");
        let source_id = SourceId::from_string("session-compound-catalog-shutdown");
        let mut service =
            SourceRegistryService::start_inner(Some(profile_root)).expect("service start");
        wait_for_catalog_ready(&service);
        service
            .submit_catalog(CatalogCommand::Register {
                source_id: source_id.clone(),
                canonical_root: source_root,
                lifecycle_generation: 1,
            })
            .expect("catalog registration");
        wait_for_catalog_event(&service, |event| {
            matches!(
                event,
                CatalogEvent::Registered {
                    source_id: registered,
                    lifecycle_generation: 1,
                } if registered == &source_id
            )
        });
        service
            .submit_catalog(CatalogCommand::RequestAudit {
                source_id: source_id.clone(),
                lifecycle_generation: 1,
            })
            .expect("catalog audit");
        let snapshot = match wait_for_catalog_event(&service, |event| {
            matches!(event, CatalogEvent::AuditCommitted { .. })
        }) {
            CatalogEvent::AuditCommitted { snapshot } => snapshot,
            _ => unreachable!(),
        };
        let curation_path =
            yield_library::sample_sources::reconciliation::RootRelativePath::try_from_path(
                PathBuf::from("one.wav"),
            )
            .expect("curation path");
        service
            .submit_catalog(CatalogCommand::ApplyCurationOperation {
                source_id: source_id.clone(),
                lifecycle_generation: 1,
                fence: SourceCurationOperationFence::ExactRevision(snapshot.committed_revision),
                root_identity: snapshot.root_identity.clone(),
                request_id: 9_100,
                batches: vec![vec![SourceCurationUpdate::SetRating {
                    path: curation_path,
                    rating: yield_library::sample_sources::Rating::KEEP_3,
                }]],
            })
            .expect("compound curation admission");
        service
            .submit_catalog(CatalogCommand::Shutdown)
            .expect("catalog shutdown admission");
        assert_eq!(
            service.submit_catalog(CatalogCommand::ReadSnapshot {
                source_id: source_id.clone(),
                expected_revision: None,
            }),
            Err(CatalogSubmitError::Closed)
        );

        let mut pending_catalog_events = Vec::new();
        let operation = wait_for_catalog_event_retaining_unmatched(
            &service,
            &mut pending_catalog_events,
            |event| {
                matches!(
                    event,
                    CatalogEvent::CurationOperationCompleted {
                        request_id: 9_100,
                        ..
                    }
                )
            },
            Duration::from_secs(120),
            "compound curation completion",
        );
        match operation {
            CatalogEvent::CurationOperationCompleted { outcome, .. } => {
                assert!(outcome.is_success());
                assert_eq!(
                    outcome.initial_revision(),
                    Some(snapshot.committed_revision)
                );
                assert_eq!(
                    outcome.final_revision(),
                    Some(snapshot.committed_revision + 1)
                );
                assert_eq!(outcome.committed_batches().len(), 1);
            }
            _ => unreachable!(),
        }
        let closed = wait_for_catalog_event_retaining_unmatched(
            &service,
            &mut pending_catalog_events,
            |event| matches!(event, CatalogEvent::Closed),
            Duration::from_secs(120),
            "catalog shutdown event",
        );
        assert!(matches!(closed, CatalogEvent::Closed));
        assert!(matches!(
            service.try_recv_catalog(),
            Err(TryRecvError::Disconnected)
        ));
        service
            .shutdown()
            .expect("session shutdown after catalog close");
    }

    #[test]
    fn preferences_round_trip_with_missing_theme_source_path() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile_root = profile_root(&temp);
        let mut service =
            SourceRegistryService::start_portable_registry_for_test(profile_root.clone())
                .expect("service start");
        wait_for_preference_snapshot(&service, |preferences| {
            preferences == &UiPreferences::default()
        });

        let mut theme = yield_design::Theme::default();
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

        let mut reopened = SourceRegistryService::start_portable_registry_for_test(profile_root)
            .expect("reopen service");
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
            yield_design::validate_theme(expected_theme).expect("theme valid")
        );
        assert_eq!(restored.theme_source_path, Some(source_path));
        reopened.shutdown().expect("reopen preference shutdown");
    }

    // The session owner requires the qualified physical reserve provider at bootstrap.
    // Linux deliberately reports Unsupported; pure preference/receipt fixtures run there.
    #[cfg(target_os = "macos")]
    #[test]
    fn approved_user_shader_survives_profile_owner_shutdown_and_reopen() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile_root = profile_root(&temp);
        let mut service =
            SourceRegistryService::start_inner(Some(profile_root.clone())).expect("service start");
        wait_for_preference_snapshot(&service, |preferences| {
            preferences == &UiPreferences::default()
        });

        let source = "@fragment fn post_fragment() -> vec4<f32> { return vec4<f32>(1.0); }";
        let fingerprint =
            user_shader_content_fingerprint(source).expect("source fingerprint for confirmation");
        let patches = [
            PreferencePatch::ImportUserShader {
                source: source.to_owned(),
            },
            PreferencePatch::ApproveUserShader {
                expected_fingerprint: fingerprint,
            },
            PreferencePatch::SetUserShaderEnabled(true),
        ];
        let mut saved_revision = 0;
        for patch in patches {
            let request = service
                .patch_preferences(patch)
                .expect("submit profile-owned shader command");
            let applied = wait_for_preference_event(&service, |event| {
                matches!(
                    event,
                    PreferenceEvent::RequestApplied { request: applied, .. }
                        if *applied == request
                )
            });
            if let PreferenceEvent::RequestApplied { revision, .. } = applied {
                saved_revision = revision.get();
            }
        }
        wait_for_preference_event(
            &service,
            |event| matches!(event, PreferenceEvent::Persisted { revision } if revision.get() >= saved_revision),
        );
        service.shutdown().expect("owner flush and shutdown");

        let mut reopened =
            SourceRegistryService::start_inner(Some(profile_root)).expect("reopen service");
        let restored = wait_for_preference_snapshot(&reopened, |preferences| {
            preferences.enabled_user_shader_source() == Some(source)
        });
        assert_eq!(restored.enabled_user_shader_source(), Some(source));
        assert_eq!(
            restored
                .user_shader
                .as_ref()
                .and_then(|shader| shader.approved_fingerprint.as_deref()),
            Some(
                user_shader_content_fingerprint(source)
                    .expect("recompute restored fingerprint")
                    .as_str()
            )
        );
        reopened.shutdown().expect("reopen owner shutdown");
    }

    #[test]
    fn collection_slot_name_survives_profile_restart_without_leaking_to_another_profile() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let first_profile = profile_root(&temp);
        let second_profile = temp
            .path()
            .join(".yield")
            .join(PROFILE_DIR_NAME)
            .join("other-profile");
        fs::create_dir_all(&second_profile).expect("other profile root");
        let second_profile = fs::canonicalize(second_profile).expect("canonical other profile");
        let mut service =
            SourceRegistryService::start_inner(Some(first_profile.clone())).expect("first start");
        wait_for_preference_snapshot(&service, |preferences| {
            preferences == &UiPreferences::default()
        });
        let request = service
            .patch_preferences(PreferencePatch::SetCollectionName {
                slot: 4,
                name: String::from("Textures"),
            })
            .expect("rename request");
        wait_for_preference_event(
            &service,
            |event| matches!(event, PreferenceEvent::RequestApplied { request: applied, .. } if *applied == request),
        );
        wait_for_preference_event(
            &service,
            |event| matches!(event, PreferenceEvent::Persisted { revision } if revision.get() > 0),
        );
        service.shutdown().expect("first shutdown");

        let mut reopened =
            SourceRegistryService::start_inner(Some(first_profile)).expect("reopen first");
        let restored = wait_for_preference_snapshot(&reopened, |preferences| {
            preferences.collection_names[4].as_deref() == Some("Textures")
        });
        assert!(
            restored
                .collection_names
                .iter()
                .enumerate()
                .all(|(slot, name)| slot == 4 || name.is_none())
        );
        reopened.shutdown().expect("reopen shutdown");

        let mut other =
            SourceRegistryService::start_inner(Some(second_profile)).expect("other profile start");
        let isolated = wait_for_preference_snapshot(&other, |preferences| {
            preferences == &UiPreferences::default()
        });
        assert!(isolated.collection_names.iter().all(Option::is_none));
        other.shutdown().expect("other shutdown");
    }

    #[test]
    fn immediate_shutdown_persists_latest_output_volume() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile_root = profile_root(&temp);
        let mut service =
            SourceRegistryService::start_portable_registry_for_test(profile_root.clone())
                .expect("service start");
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

        let mut reopened = SourceRegistryService::start_portable_registry_for_test(profile_root)
            .expect("reopen service");
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
            SourceRegistryService::start_portable_registry_for_test(profile_root.clone())
                .expect("service start");
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

        let mut reopened = SourceRegistryService::start_portable_registry_for_test(profile_root)
            .expect("reopen service");
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
        let connection = yield_library::sample_sources::library::open_connection()
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
        let connection = yield_library::sample_sources::library::open_connection()
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
            SourceRegistryService::start_portable_registry_for_test(profile_root.clone())
                .expect("service start");
        wait_for_event(&initial, |event| {
            matches!(event, RegistryEvent::Snapshot { .. })
        });
        wait_for_preference_snapshot(&initial, |preferences| {
            preferences == &UiPreferences::default()
        });
        initial.shutdown().expect("initial shutdown");

        let app_root_guard = AppRootGuard::set(profile_root.clone()).expect("app root");
        let connection = yield_library::sample_sources::library::open_connection()
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

        let mut service = SourceRegistryService::start_portable_registry_for_test(profile_root)
            .expect("reopen service");
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
                    Err(SubmitError::MaintenanceActive) => {
                        panic!("maintenance unexpectedly blocked source admission")
                    }
                    Err(SubmitError::Closed) => panic!("session closed while queueing lookups"),
                }
            }
        }

        let preference_request = loop {
            match service.patch_preferences(PreferencePatch::SetLoopEnabled(true)) {
                Ok(request) => break request,
                Err(SubmitError::Full) => thread::sleep(Duration::from_millis(1)),
                Err(SubmitError::MaintenanceActive) => {
                    panic!("maintenance unexpectedly blocked preference admission")
                }
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
    fn competing_profile_restarts_only_after_orderly_volume_handoff() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let owner_root = profile_root(&temp);
        let competing_root = temp
            .path()
            .join(".yield")
            .join(PROFILE_DIR_NAME)
            .join("competing-profile");
        fs::create_dir_all(&competing_root).expect("competing profile root");
        let competing_root = fs::canonicalize(competing_root).expect("canonical contender");

        let mut owner =
            SourceRegistryService::start_inner(Some(owner_root.clone())).expect("owner service");
        wait_for_session_owner_slots(&owner);
        let owner_journal = journal_database_path(&owner_root);
        assert!(owner_journal.is_file());

        let mut contender = SourceRegistryService::start_inner(Some(competing_root.clone()))
            .expect("contender service");
        wait_for_event(&contender, |event| {
            matches!(
                event,
                RegistryEvent::PersistenceUnavailable {
                    error: SessionError::Persistence(reason),
                    ..
                } if reason.contains("volume ownership is held by another process")
            )
        });
        let catalog_status = contender.catalog_status();
        assert!(
            matches!(
                &catalog_status,
                CatalogStatus::Unavailable { reason }
                    if reason.contains("volume ownership is held by another process")
            ),
            "catalog status: {catalog_status:?}"
        );
        assert!(matches!(
            contender.vocabulary_status(),
            VocabularyStatus::Unavailable { reason }
                if reason.contains("volume ownership is held by another process")
        ));
        assert!(matches!(
            contender.submit_catalog(CatalogCommand::Register {
                source_id: SourceId::from_string("competing-profile-source"),
                canonical_root: competing_root.clone(),
                lifecycle_generation: 1,
            }),
            Err(CatalogSubmitError::Unavailable(reason))
                if reason.contains("volume ownership is held by another process")
        ));
        assert!(matches!(
            contender.lookup_profile_tag_categories(vec![String::from("competing-profile-tag")]),
            Err(VocabularySubmitError::Unavailable(reason))
                if reason.contains("volume ownership is held by another process")
        ));
        assert!(matches!(
            contender.shutdown(),
            Err(ShutdownError::Session(SessionError::Persistence(reason)))
                if reason.contains("volume ownership is held by another process")
        ));
        assert!(owner_journal.is_file());

        owner.shutdown().expect("clean owner handoff");
        let mut restarted = SourceRegistryService::start_inner(Some(competing_root))
            .expect("competing profile restart");
        wait_for_session_owner_slots(&restarted);
        wait_for_catalog_ready(&restarted);
        restarted.shutdown().expect("competing profile shutdown");
        let _owner_scope = AppRootGuard::set(owner_root).expect("owner profile after handoff");
        let guard = WritableProfileGuard::acquire_current().expect("owner profile remains usable");
        let journal = OperationJournalStore::open(&guard).expect("owner journal remains readable");
        assert!(matches!(
            journal
                .recovery_readiness()
                .expect("owner recovery readiness"),
            OperationJournalRecoveryReadiness::Clear
        ));
    }

    #[test]
    fn cross_process_profile_owner_conflict_leaves_session_slots_uninstalled() {
        let temp = TempDir::new().expect("config base");
        let _base_guard = ConfigBaseGuard::set(fs::canonicalize(temp.path()).expect("base"));
        let profile_root = profile_root(&temp);
        let mut owner = spawn_session_owner_helper(&profile_root);
        wait_for_session_owner_ready(&mut owner);
        let journal_path = journal_database_path(&profile_root);
        let journal_before = fs::read(&journal_path).expect("owner-created journal");

        {
            let _app_root_guard = AppRootGuard::set(profile_root.clone()).expect("app root");
            let error = WritableProfileGuard::acquire_current()
                .expect_err("the child process should own the profile");
            assert!(matches!(
                error,
                ProfileOwnershipError::ProfileOwnedByAnotherProcess { path }
                    if path == profile_root.join("profile-owner.lock")
            ));
        }

        let mut contender = SourceRegistryService::start_inner(Some(profile_root.clone()))
            .expect("contender service");
        wait_for_event(&contender, |event| {
            matches!(
                event,
                RegistryEvent::PersistenceUnavailable {
                    error: SessionError::Persistence(reason),
                    ..
                } if reason.contains("profile is owned by another process")
            )
        });
        assert!(matches!(
            contender.catalog_status(),
            CatalogStatus::Unavailable { ref reason }
                if reason.contains("profile is owned by another process")
        ));
        assert!(matches!(
            contender.vocabulary_status(),
            VocabularyStatus::Unavailable { ref reason }
                if reason.contains("profile is owned by another process")
        ));
        assert!(
            matches!(
                contender.submit_catalog(CatalogCommand::Register {
                    source_id: SourceId::from_string("non-owner-catalog"),
                    canonical_root: profile_root.clone(),
                    lifecycle_generation: 1,
                }),
                Err(CatalogSubmitError::Unavailable(reason))
                    if reason.contains("profile is owned by another process")
            ),
            "catalog commands must remain unavailable for a non-owner session"
        );
        assert!(
            matches!(
                contender.lookup_profile_tag_categories(vec![String::from("non-owner-vocabulary")]),
                Err(VocabularySubmitError::Unavailable(reason))
                    if reason.contains("profile is owned by another process")
            ),
            "global writer vocabulary commands must remain unavailable for a non-owner session"
        );
        assert!(matches!(
            contender.shutdown(),
            Err(ShutdownError::Session(SessionError::Persistence(reason)))
                if reason.contains("profile is owned by another process")
        ));
        assert_eq!(
            fs::read(&journal_path).expect("journal after losing process"),
            journal_before,
            "losing process must not open or mutate the journal"
        );

        release_session_owner_helper(&mut owner);
        wait_for_session_owner_exit(&mut owner);
    }

    #[test]
    #[ignore = "spawned by the cross-process session ownership test"]
    fn session_owner_process_helper() {
        let profile_root = PathBuf::from(
            std::env::var_os(SESSION_OWNER_PROFILE_ROOT_ENV)
                .expect("session owner helper profile root"),
        );
        let mut service = SourceRegistryService::start_inner(Some(profile_root))
            .expect("session owner helper service");
        wait_for_session_owner_slots(&service);
        println!("{SESSION_OWNER_READY_LINE}");
        io::stdout()
            .flush()
            .expect("flush session owner helper handshake");

        let mut command = String::new();
        io::stdin()
            .read_line(&mut command)
            .expect("read session owner helper command");
        match command.trim() {
            "release" => service.shutdown().expect("session owner helper shutdown"),
            _ => panic!("unexpected session owner helper command"),
        }
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
            service.vocabulary_status(),
            VocabularyStatus::Unavailable { .. }
        ));
        assert!(matches!(
            service.lookup_profile_tag_categories(vec![String::from("kick")]),
            Err(VocabularySubmitError::Unavailable(_))
        ));
        assert!(matches!(
            service.delete_profile_tag_vocabulary(0, String::from("kick")),
            Err(VocabularySubmitError::Unavailable(_))
        ));
        assert!(matches!(
            service.shutdown(),
            Err(ShutdownError::Session(SessionError::Persistence(_)))
        ));
        assert!(invalid_root.is_file());
        assert!(!temp.path().join(".yield").exists());
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
            maintenance: MaintenanceService::new(Arc::new(MaintenanceAdmission::new())),
            catalog_slot: Arc::new(Mutex::new(CatalogSlot::loading())),
            vocabulary_slot: Arc::new(Mutex::new(VocabularySlot::loading())),
            writer: None,
            catalog: None,
            journal: None,
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
            maintenance: MaintenanceService::new(Arc::new(MaintenanceAdmission::new())),
            catalog_slot: Arc::new(Mutex::new(CatalogSlot::loading())),
            vocabulary_slot: Arc::new(Mutex::new(VocabularySlot::loading())),
            writer: None,
            catalog: None,
            journal: None,
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

    fn wait_for_vocabulary_available(service: &SourceRegistryService) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match service.vocabulary_status() {
                VocabularyStatus::Available => return,
                VocabularyStatus::Initializing => {
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting for vocabulary availability"
                    );
                    thread::sleep(Duration::from_millis(5));
                }
                VocabularyStatus::Unavailable { reason } => {
                    panic!("vocabulary startup failed: {reason}");
                }
                VocabularyStatus::Closed => panic!("vocabulary closed during startup"),
            }
        }
    }

    fn receive_vocabulary<T>(request: &mut VocabularyRequest<T>) -> Result<T, LibraryError> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match request.try_recv() {
                Ok(result) => return result,
                Err(TryRecvError::Empty) => {
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting for vocabulary result"
                    );
                    thread::sleep(Duration::from_millis(5));
                }
                Err(TryRecvError::Disconnected) => {
                    panic!("vocabulary result channel disconnected")
                }
            }
        }
    }
}
