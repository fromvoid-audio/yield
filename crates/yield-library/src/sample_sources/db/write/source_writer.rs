//! Source-owned revision-fenced curation commands.
//!
//! This module is deliberately narrower than the operation journal.  A `SourceWriter` owns one
//! already-authorized [`SourceDatabase`] and records source-participant receipts in that same
//! database transaction.  The live revision/fence values below are observations of the retained
//! owner; their persisted DTOs never construct or reacquire an authority.

use super::super::{SourceReferenceFence, SourceReferenceFenceError};
mod collection_history_selection;
pub use collection_history_selection::{
    SourceCollectionHistoryChange, SourceCollectionHistorySelection,
};
mod snapshot_metrics;
pub use snapshot_metrics::SourceSnapshotQueueMetrics;
use snapshot_metrics::{SnapshotCounters, SnapshotSender};
mod feature_maintenance;
mod feature_maintenance_metrics;
pub use feature_maintenance::{FeaturePruneCursor, FeaturePrunePage};
pub use feature_maintenance_metrics::FeatureMaintenanceMetrics;
mod feature_metrics;
pub use feature_metrics::SourceFeaturePublicationMetrics;
mod feature_rebuild_metrics;
mod layout_capture;
mod layout_generation;
pub use layout_generation::{
    SOURCE_LAYOUT_CANONICAL_MAX_BYTES, SOURCE_LAYOUT_GENERATION_KEY, SourceLayoutComputedPoint,
    SourceLayoutComputedView, SourceLayoutGeneration, SourceLayoutGenerationError,
    SourceLayoutGenerationRow, SourcePreparedLayoutGeneration,
};
mod manifest_enrollment;
pub use feature_rebuild_metrics::FeatureRebuildMetrics;
mod layout_metadata_budget;
mod layout_metadata_policy;
pub use layout_metadata_budget::{SOURCE_LAYOUT_GENERATION_MAX_BYTES, SourceLayoutMetadataBudget};
pub use layout_metadata_policy::SourceLayoutMetadataPolicy;
mod tag_admission_observation;
pub use layout_capture::{
    SOURCE_LAYOUT_ALGORITHM_V1, SourceLayoutCapture, SourceLayoutCaptureError, SourceLayoutFeature,
};
mod tag_metadata_budget;
mod tag_metadata_policy;
mod tag_wal_preparation;
pub use tag_admission_observation::SourceTagAdmissionObservation;
pub use tag_wal_preparation::{SourceTagWalPreparation, SourceTagWalPreparationFailure};
mod wal_reservation;
pub use tag_metadata_budget::SourceTagMetadataBudget;
pub use tag_metadata_policy::SourceTagMetadataPolicy;
pub use wal_reservation::{
    SourceWalPreparation, SourceWalPreparationError, SourceWalPreparationFailure,
};
#[path = "readiness_targets.rs"]
mod readiness_targets;
pub use manifest_enrollment::{
    ManifestEnrollmentEntry, ManifestEnrollmentError, ManifestEnrollmentKind,
    ManifestEnrollmentReceipt, ManifestEnrollmentRequest, ManifestEnrollmentResult,
};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rusqlite::{Transaction, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::app_dirs::WritableProfileGuard;
use crate::operation_journal::{MAX_RECORD_BYTES, OperationId, ParticipantId};
use crate::sample_sources::SourceId;
use crate::sample_sources::db::read::browser_metadata_rows_for_paths;
use crate::sample_sources::readiness::{
    ArtifactPublishOutcome, ClaimedReadinessWork, ReadinessDeficit, ReadinessError,
    ReadinessFailureClassification, ReadinessFailureOutcome, ReadinessLeaseRenewalOutcome,
    ReadinessRetryPolicy, ReadinessTarget, ReadinessWorkMutationOutcome,
};
use crate::sample_sources::reconciliation::{
    BootstrapReplayEvidence, ReplayPriorToken, RetainedUncertainty, RootIdentity, RootRelativePath,
    SourceAuditCommit, SourceAuditRequest, WatcherContinuityProof, WatcherGeneration,
};

use super::super::replay_authority::{
    DurableReplayUncertainty, DurableReplayUncertaintyState, encode_bootstrap_checkpoint,
    encode_replay_checkpoint, parse_durable_replay_prior,
};
use super::super::util::{map_sql_error, normalize_relative_path};
use super::super::{
    BrowserFileMetadata, BrowserMetadataCursor, BrowserMetadataPage,
    META_SOURCE_WATCHER_CHECKPOINT, META_SOURCE_WATCHER_UNCERTAINTY, ManifestCommitResult, Rating,
    SampleCollection, SourceDatabase, SourceDbError, SourceDirectoryEntry,
    SourceDirectoryTruthCleanup, SourceManifestEntry, SourcePlaybackType, SourceWriteBatch,
};
use super::curation::{
    SOURCE_CURATION_BATCH_LIMIT, SourceCurationCommitReceipt, SourceCurationUpdate,
    apply_curation_updates, normalize_update_paths, validate_curation_preconditions,
    validate_update_values,
};
use super::tag_assignment_selection::{
    self, SourceTagAssignmentMutation, SourceTagAssignmentSelection,
};
use super::transaction::CurationCommitOutcome;

/// Maximum durable source-participant receipts retained by one source database.
pub const SOURCE_WRITER_RECEIPT_LIMIT: usize = 256;
/// Maximum combined request and receipt bytes retained by one source database.
pub const SOURCE_WRITER_RECEIPT_BYTES_LIMIT: usize = 16 * 1024 * 1024;
/// Maximum readiness work rows admitted in one retained-writer transaction.
const SOURCE_READINESS_JOB_BATCH_LIMIT: usize = 64;
const SOURCE_WRITER_CODEC_VERSION: u8 = 1;
const SOURCE_ID_MAX_BYTES: usize = 256;

/// Stable identity for one source-owned command within an operation participant.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SourceCommandId(Uuid);

impl SourceCommandId {
    /// Generate a new source command identity.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Wrap an existing command UUID without granting authority.
    pub fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    /// Return the wrapped UUID.
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for SourceCommandId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SourceCommandId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// A live source revision fence captured from one retained source owner.
///
/// This is intentionally not deserializable.  A caller must obtain a fresh value from
/// [`SourceWriter::current_revision`] before constructing a request, and the writer revalidates
/// the retained profile, database binding, and source lease before using it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceRevision {
    source_id: SourceId,
    root_identity: RootIdentity,
    root_binding_identity: String,
    database_identity: String,
    lifecycle_generation: u64,
    lease_epoch: u64,
    revision: u64,
}

/// Owner-issued scan position before or between authoritative manifest audits.
///
/// This is an observation, not a write grant or a completed-audit revision. Scan
/// commands must revalidate it under the retained source owner before mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceScanFence {
    source_id: SourceId,
    root_identity: RootIdentity,
    root_binding_identity: String,
    database_identity: String,
    lifecycle_generation: u64,
    lease_epoch: u64,
    revision: u64,
}

/// One bounded read result from the retained source owner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceSnapshotPage {
    /// The page was read at the named source and lifecycle revision.
    FreshAsOf {
        /// Exact owner-issued revision represented by the rows.
        revision: SourceRevision,
        /// At most 64 metadata rows and an opaque continuation.
        page: BrowserMetadataPage,
    },
    /// The requested revision or lifecycle no longer names the current source.
    Stale {
        /// Fresh owner-issued observation for restarting the read.
        current: SourceRevision,
    },
}

struct SnapshotRequest {
    expected: SourceRevision,
    after_cursor: Option<BrowserMetadataCursor>,
    requested_limit: usize,
    deadline: Instant,
    reply: mpsc::Sender<Result<SourceSnapshotPage, SourceWriterError>>,
}

struct TagSelectionRequest {
    expected: SourceRevision,
    old_label: String,
    mutation: SourceTagAssignmentMutation,
    deadline: Instant,
    reply: mpsc::Sender<Result<SourceTagAssignmentSelection, SourceWriterError>>,
}

enum SnapshotCommand {
    TagSelection(TagSelectionRequest),
    Page(SnapshotRequest),
    #[cfg(test)]
    BlockForTest {
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    },
    #[cfg(test)]
    BarrierForTest(mpsc::Sender<()>),
}

/// A bounded snapshot entrypoint that owns the source writer on one worker thread.
///
/// A timed-out request does not release or duplicate source authority. The worker keeps the
/// writer until its current OS operation finishes; subsequent requests cannot overtake it.
pub struct SourceSnapshotOwner {
    requests: Option<SnapshotSender>,
    worker: Option<JoinHandle<SourceWriter>>,
    queue_metrics: Arc<SnapshotCounters>,
}

impl SourceSnapshotOwner {
    /// Capture all bounded tag assignments on the sole worker within a two-second caller budget.
    ///
    /// Timeout retains source custody on the worker; it never grants mutation or finalization.
    pub fn capture_tag_assignment_selection(
        &self,
        expected: &SourceRevision,
        old_label: &str,
        mutation: SourceTagAssignmentMutation,
    ) -> Result<SourceTagAssignmentSelection, SourceWriterError> {
        self.capture_tag_assignment_selection_with_deadline(
            expected,
            old_label,
            mutation,
            Instant::now() + Duration::from_secs(2),
        )
    }

    pub(super) fn capture_tag_assignment_selection_with_deadline(
        &self,
        expected: &SourceRevision,
        old_label: &str,
        mutation: SourceTagAssignmentMutation,
        deadline: Instant,
    ) -> Result<SourceTagAssignmentSelection, SourceWriterError> {
        super::super::tags::validate_curation_tag_label(old_label)?;
        if let SourceTagAssignmentMutation::Rename(label) = &mutation {
            super::super::tags::validate_curation_tag_label(label)?;
        }
        if Instant::now() >= deadline {
            return Err(SourceWriterError::SnapshotBudgetExceeded);
        }
        let (reply, response) = mpsc::channel();
        let request = TagSelectionRequest {
            expected: expected.clone(),
            old_label: old_label.to_owned(),
            mutation,
            deadline,
            reply,
        };
        self.requests
            .as_ref()
            .ok_or(SourceWriterError::SnapshotOwnerUnavailable)?
            .try_send(SnapshotCommand::TagSelection(request))
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => SourceWriterError::SnapshotBudgetExceeded,
                mpsc::TrySendError::Disconnected(_) => SourceWriterError::SnapshotOwnerUnavailable,
            })?;
        let result = response
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|e| match e {
                mpsc::RecvTimeoutError::Timeout => SourceWriterError::SnapshotBudgetExceeded,
                mpsc::RecvTimeoutError::Disconnected => SourceWriterError::SnapshotOwnerUnavailable,
            })?;
        if Instant::now() >= deadline {
            return Err(SourceWriterError::SnapshotBudgetExceeded);
        }
        result
    }

    /// Transfer the existing source writer to a single snapshot worker.
    pub fn start(writer: SourceWriter) -> io::Result<Self> {
        let (requests, receiver, queue_metrics) = SnapshotSender::channel();
        let worker = thread::Builder::new()
            .name("yield-source-snapshot".to_owned())
            .spawn(move || {
                while let Ok(envelope) = receiver.recv() {
                    let command = envelope.dispatch();
                    match command {
                        SnapshotCommand::TagSelection(request) => {
                            let result = writer.capture_tag_assignment_selection_with_deadline(
                                &request.expected,
                                &request.old_label,
                                request.mutation,
                                request.deadline,
                            );
                            let _ = request.reply.send(result);
                        }
                        SnapshotCommand::Page(request) => {
                            if Instant::now() >= request.deadline {
                                let _ = request
                                    .reply
                                    .send(Err(SourceWriterError::SnapshotBudgetExceeded));
                                continue;
                            }
                            let result = writer.read_browser_metadata_page_with_deadline(
                                &request.expected,
                                request.after_cursor.as_ref(),
                                request.requested_limit,
                                request.deadline,
                            );
                            let _ = request.reply.send(result);
                        }
                        #[cfg(test)]
                        SnapshotCommand::BlockForTest { entered, release } => {
                            let _ = entered.send(());
                            let _ = release.recv();
                        }
                        #[cfg(test)]
                        SnapshotCommand::BarrierForTest(reply) => {
                            let _ = reply.send(());
                        }
                    }
                }
                writer
            })?;
        Ok(Self {
            requests: Some(requests),
            worker: Some(worker),
            queue_metrics,
        })
    }

    /// Copy fixed aggregate channel observations without SQL, request labels or event traffic.
    /// These independently sampled counters confer no freshness or source authority.
    pub fn queue_metrics(&self) -> SourceSnapshotQueueMetrics {
        self.queue_metrics.snapshot()
    }

    /// Return within the two-second request budget even if an OS identity check blocks.
    pub fn read_browser_metadata_page(
        &self,
        expected: &SourceRevision,
        after_cursor: Option<&BrowserMetadataCursor>,
        requested_limit: usize,
    ) -> Result<SourceSnapshotPage, SourceWriterError> {
        self.read_browser_metadata_page_with_deadline(
            expected,
            after_cursor,
            requested_limit,
            Instant::now() + Duration::from_secs(2),
        )
    }

    pub(super) fn read_browser_metadata_page_with_deadline(
        &self,
        expected: &SourceRevision,
        after_cursor: Option<&BrowserMetadataCursor>,
        requested_limit: usize,
        deadline: Instant,
    ) -> Result<SourceSnapshotPage, SourceWriterError> {
        // Expired requests must not occupy the sole pending slot or clone their payload.
        if Instant::now() >= deadline {
            return Err(SourceWriterError::SnapshotBudgetExceeded);
        }
        if after_cursor.is_some_and(|cursor| cursor.as_str().len() > 16 * 1024 * 1024) {
            return Err(SourceWriterError::SnapshotBudgetExceeded);
        }
        let (reply, receiver) = mpsc::channel();
        let request = SnapshotRequest {
            expected: expected.clone(),
            after_cursor: after_cursor.cloned(),
            requested_limit,
            deadline,
            reply,
        };
        self.requests
            .as_ref()
            .ok_or(SourceWriterError::SnapshotOwnerUnavailable)?
            .try_send(SnapshotCommand::Page(request))
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => SourceWriterError::SnapshotBudgetExceeded,
                mpsc::TrySendError::Disconnected(_) => SourceWriterError::SnapshotOwnerUnavailable,
            })?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        let result = receiver
            .recv_timeout(remaining)
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => SourceWriterError::SnapshotBudgetExceeded,
                mpsc::RecvTimeoutError::Disconnected => SourceWriterError::SnapshotOwnerUnavailable,
            })?;
        if Instant::now() >= deadline {
            return Err(SourceWriterError::SnapshotBudgetExceeded);
        }
        result
    }

    /// Stop accepting requests and return a join handle for orderly owner retirement.
    /// Joining may block while an OS operation finishes; never join on the UI thread.
    pub fn begin_retirement(mut self) -> JoinHandle<SourceWriter> {
        self.requests.take();
        self.worker.take().expect("snapshot worker exists")
    }

    #[cfg(test)]
    pub(super) fn block_for_tests(&self, entered: mpsc::Sender<()>, release: mpsc::Receiver<()>) {
        self.requests
            .as_ref()
            .expect("snapshot request channel exists")
            .try_send(SnapshotCommand::BlockForTest { entered, release })
            .unwrap_or_else(|_| panic!("snapshot worker accepts test gate"));
    }

    #[cfg(test)]
    pub(super) fn wait_idle_for_tests(&self) {
        let (reply, receiver) = mpsc::channel();
        self.requests
            .as_ref()
            .expect("snapshot request channel exists")
            .send(SnapshotCommand::BarrierForTest(reply))
            .unwrap_or_else(|_| panic!("snapshot worker accepts test barrier"));
        receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("snapshot worker reaches test barrier");
    }
}

struct SnapshotProgressGuard<'a>(&'a rusqlite::Connection);

impl<'a> SnapshotProgressGuard<'a> {
    fn install(connection: &'a rusqlite::Connection, deadline: std::time::Instant) -> Self {
        connection.progress_handler(1_000, Some(move || std::time::Instant::now() >= deadline));
        Self(connection)
    }
}

impl Drop for SnapshotProgressGuard<'_> {
    fn drop(&mut self) {
        self.0.progress_handler(0, None::<fn() -> bool>);
    }
}

impl SourceRevision {
    /// Borrow the source identity named by the owner.
    pub fn source_id(&self) -> &SourceId {
        &self.source_id
    }

    /// Borrow the audited physical source-root identity.
    pub fn root_identity(&self) -> &RootIdentity {
        &self.root_identity
    }

    /// Borrow the retained database-file identity.
    pub fn database_identity(&self) -> &str {
        &self.database_identity
    }

    /// Borrow the retained live root-binding identity.
    pub fn root_binding_identity(&self) -> &str {
        &self.root_binding_identity
    }

    /// Return the catalog lifecycle generation supplied when the writer was constructed.
    pub const fn lifecycle_generation(&self) -> u64 {
        self.lifecycle_generation
    }

    /// Return the positive source-lease epoch.
    pub const fn lease_epoch(&self) -> u64 {
        self.lease_epoch
    }

    /// Return the exact source revision.
    pub const fn revision(&self) -> u64 {
        self.revision
    }
}

impl SourceScanFence {
    /// Borrow the retained source identity.
    pub fn source_id(&self) -> &SourceId {
        &self.source_id
    }

    /// Borrow the physical root identity validated by the source lease.
    pub fn root_identity(&self) -> &RootIdentity {
        &self.root_identity
    }

    /// Return the catalog lifecycle generation supplied to the owner.
    pub const fn lifecycle_generation(&self) -> u64 {
        self.lifecycle_generation
    }

    /// Return the positive source lease epoch.
    pub const fn lease_epoch(&self) -> u64 {
        self.lease_epoch
    }

    /// Return the observed source revision, which may precede the first full audit.
    pub const fn revision(&self) -> u64 {
        self.revision
    }
}

/// Identity echoed by a source-writer receipt or retained uncertain result.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SourceWriterCommandIdentity {
    operation_id: OperationId,
    participant_id: ParticipantId,
    command_id: SourceCommandId,
}

impl SourceWriterCommandIdentity {
    /// Construct a command identity observation.
    pub const fn new(
        operation_id: OperationId,
        participant_id: ParticipantId,
        command_id: SourceCommandId,
    ) -> Self {
        Self {
            operation_id,
            participant_id,
            command_id,
        }
    }

    /// Return the operation identity.
    pub const fn operation_id(self) -> OperationId {
        self.operation_id
    }

    /// Return the participant identity.
    pub const fn participant_id(self) -> ParticipantId {
        self.participant_id
    }

    /// Return the stable command identity.
    pub const fn command_id(self) -> SourceCommandId {
        self.command_id
    }
}

/// One source command with an owner-issued expected fence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceWriterRequest {
    identity: SourceWriterCommandIdentity,
    expected: StoredSourceRevision,
    updates: Vec<StoredCurationUpdate>,
    canonical_bytes: Vec<u8>,
}

impl SourceWriterRequest {
    /// Build a bounded request from a live source revision and existing curation updates.
    pub fn new(
        operation_id: OperationId,
        participant_id: ParticipantId,
        command_id: SourceCommandId,
        expected: SourceRevision,
        updates: Vec<SourceCurationUpdate>,
    ) -> Result<Self, SourceWriterError> {
        if updates.is_empty() || updates.len() > SOURCE_CURATION_BATCH_LIMIT {
            return Err(SourceWriterError::InvalidRequest(
                "curation update count is outside the bounded source command limit",
            ));
        }
        validate_source_id(&expected.source_id)?;
        validate_revision_bounds(&expected)?;
        validate_update_values(&updates)?;
        let updates = updates
            .iter()
            .map(StoredCurationUpdate::from_live)
            .collect::<Result<Vec<_>, _>>()?;
        let request = StoredSourceWriterRequest {
            version: SOURCE_WRITER_CODEC_VERSION,
            operation_id,
            participant_id,
            command_id,
            source_id: expected.source_id.as_str().to_owned(),
            expected: StoredSourceRevision::from_live(&expected),
            updates,
        };
        let canonical_bytes = encode_bounded(&request)?;
        Ok(Self {
            identity: SourceWriterCommandIdentity::new(operation_id, participant_id, command_id),
            expected: request.expected.clone(),
            updates: request.updates.clone(),
            canonical_bytes,
        })
    }

    /// Return the stable source command identity.
    pub const fn identity(&self) -> SourceWriterCommandIdentity {
        self.identity
    }

    /// Return the exact canonical request bytes used for durable duplicate comparison.
    pub(crate) fn canonical_bytes(&self) -> &[u8] {
        &self.canonical_bytes
    }

    fn expected(&self) -> &StoredSourceRevision {
        &self.expected
    }

    fn updates(&self) -> Result<Vec<SourceCurationUpdate>, SourceWriterError> {
        self.updates
            .iter()
            .map(StoredCurationUpdate::to_live)
            .collect()
    }
}

/// Source-participant receipt returned after a committed curation transaction or exact replay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceWriterReceipt {
    identity: SourceWriterCommandIdentity,
    revision: SourceRevision,
    curation: SourceCurationCommitReceipt,
}

impl SourceWriterReceipt {
    /// Return the command identity that produced this receipt.
    pub const fn identity(&self) -> SourceWriterCommandIdentity {
        self.identity
    }

    /// Borrow the committed source revision observation.
    pub fn source_revision(&self) -> &SourceRevision {
        &self.revision
    }

    /// Borrow the exact metadata delta returned by the curation transaction.
    pub fn curation(&self) -> &SourceCurationCommitReceipt {
        &self.curation
    }
}

/// Fail-closed errors from the source-owned revision/receipt boundary.
#[derive(Debug, Error)]
pub enum SourceWriterError {
    /// Unknown/unavailable source metadata allocation policy; no edits or capacity authority.
    #[error("source metadata allocation bound is not defined for this live owner")]
    MetadataBoundNotDefined,
    /// Held feature source input could not be admitted or validated.
    #[error(transparent)]
    FeatureInput(#[from] crate::sample_sources::readiness::SourceFeatureInputError),
    /// Existing source-database validation or transaction failure.
    #[error(transparent)]
    Database(#[from] SourceDbError),
    /// The retained source's typed readiness store rejected a work transition.
    #[error(transparent)]
    Readiness(#[from] ReadinessError),
    /// The request could not be represented by the bounded source codec.
    #[error("invalid source writer request: {0}")]
    InvalidRequest(&'static str),
    /// The expected source revision no longer matches the retained source.
    #[error("source revision is stale: expected {expected}, actual {actual}")]
    StaleRevision {
        /// Revision carried by the rejected request.
        expected: u64,
        /// Revision observed under the live source owner.
        actual: u64,
    },
    /// One physical or lifecycle fence no longer matches the retained source owner.
    #[error("source writer fence is stale")]
    StaleFence,
    /// A durable command identity already names different canonical request bytes.
    #[error("source writer command identity conflicts with a retained receipt")]
    ConflictingCommand {
        /// Identity whose existing receipt was not equivalent.
        identity: SourceWriterCommandIdentity,
    },
    /// A retained receipt belongs to a prior source lifecycle.
    #[error("source writer command belongs to a superseded source lifecycle")]
    SupersededLifecycle {
        /// Identity retained for reconciliation.
        identity: SourceWriterCommandIdentity,
    },
    /// A stored receipt could not be decoded or failed its structural checks.
    #[error("source writer receipt is malformed")]
    MalformedReceipt,
    /// The source receipt relation reached its bounded retained-history budget.
    #[error("source writer receipt capacity is exhausted")]
    CapacityExceeded,
    /// A bounded owner read exceeded its row, byte, or elapsed-time budget.
    #[error("source snapshot read exceeded its bounded budget")]
    SnapshotBudgetExceeded,
    /// The sole snapshot worker exited before completing the request.
    #[error("source snapshot owner is unavailable")]
    SnapshotOwnerUnavailable,
    /// The replay proof does not extend the exact retained durable checkpoint.
    #[error("watcher replay checkpoint proof or prior does not match the retained source")]
    ReplayCheckpointMismatch,
    /// The source transaction may have committed, but exact checkpoint readback failed.
    #[error("watcher replay checkpoint outcome is uncertain at event {event_id}")]
    ReplayCheckpointOutcomeUncertain {
        /// Event ID to use for an exact persisted checkpoint readback or retry.
        event_id: u64,
    },
    /// A source-owned uncertainty marker is malformed or belongs to another source lane.
    #[error("watcher replay uncertainty does not match the retained source")]
    ReplayUncertaintyMismatch,
    /// The uncertainty write may have committed but its exact readback failed.
    #[error("watcher replay uncertainty outcome is uncertain")]
    ReplayUncertaintyOutcomeUncertain,
    /// The first replay prior did not match the completed audit and live source lane.
    #[error("watcher replay bootstrap evidence does not match the retained source audit")]
    ReplayBootstrapMismatch,
    /// Commit may have happened, but a matching durable replay prior could not be read back.
    #[error("watcher replay bootstrap outcome is uncertain at event {event_id}")]
    ReplayBootstrapOutcomeUncertain {
        /// Exact native event cursor whose outcome needs readback.
        event_id: u64,
    },
    /// Commit acknowledgement or readback became uncertain after durable work could have run.
    #[error("source writer commit outcome is uncertain")]
    Uncertain {
        /// Exact command identity retained for reconciliation.
        identity: SourceWriterCommandIdentity,
        /// Canonical request evidence retained for an exact retry/readback.
        canonical_request: Vec<u8>,
    },
}

/// Owner of one already-profile-bound source database.
pub struct SourceWriter {
    feature_publication_metrics: SourceFeaturePublicationMetrics,
    reference_liveness: Arc<()>,
    feature_rebuild_metrics: FeatureRebuildMetrics,
    feature_maintenance_metrics: FeatureMaintenanceMetrics,
    database: SourceDatabase,
    source_id: SourceId,
    lifecycle_generation: u64,
    #[cfg(test)]
    fault: Option<SourceWriterFault>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SourceWriterFault {
    BeforeCommit,
    OversizedReceipt,
    ReceiptAtRecordLimit,
    RollbackUncertain,
    AfterCommit,
}

impl SourceWriter {
    /// Persist the first unresolved source-wide uncertainty marker under the exact source fence.
    ///
    /// One source-wide marker is enough to require a complete audit. Later markers widen its
    /// bounded reasons and advance its revision watermark, so an earlier audit cannot clear
    /// uncertainty observed afterward. A complete, matching bootstrap audit retires it.
    pub fn retain_watcher_replay_uncertainty(
        &self,
        expected: &SourceScanFence,
        marker: &RetainedUncertainty,
    ) -> Result<DurableReplayUncertainty, SourceWriterError> {
        self.validate_scan_fence(expected)?;
        let requested = DurableReplayUncertainty::from_marker(
            marker,
            expected.source_id(),
            expected.root_identity(),
            expected.lifecycle_generation(),
            expected.revision(),
        )
        .ok_or(SourceWriterError::ReplayUncertaintyMismatch)?;
        let encoded = requested.encode().ok_or(SourceWriterError::InvalidRequest(
            "watcher replay uncertainty exceeds the bounded metadata size",
        ))?;
        let mut batch = self.database.write_batch()?;
        if !batch.matches_revision(expected.revision())? {
            return Err(SourceWriterError::StaleRevision {
                expected: expected.revision(),
                actual: batch.get_revision()?,
            });
        }
        let retained =
            if let Some(stored) = batch.coordination_metadata(META_SOURCE_WATCHER_UNCERTAINTY)? {
                let previous = DurableReplayUncertainty::decode(&stored)
                    .ok_or(SourceWriterError::ReplayUncertaintyMismatch)?;
                if previous.source_revision() > expected.revision() {
                    return Err(SourceWriterError::ReplayUncertaintyMismatch);
                }
                let retained = previous
                    .clone()
                    .merged_with(&requested)
                    .ok_or(SourceWriterError::ReplayUncertaintyMismatch)?;
                if retained != previous {
                    let encoded = retained.encode().ok_or(SourceWriterError::InvalidRequest(
                        "watcher replay uncertainty exceeds the bounded metadata size",
                    ))?;
                    batch.set_metadata(META_SOURCE_WATCHER_UNCERTAINTY, &encoded)?;
                }
                retained
            } else {
                batch.set_metadata(META_SOURCE_WATCHER_UNCERTAINTY, &encoded)?;
                requested
            };
        batch.commit_auxiliary_state()?;
        match self.database.read_durable_replay_uncertainty() {
            Ok(DurableReplayUncertaintyState::Retained(readback)) if readback == retained => {
                Ok(readback)
            }
            _ => Err(SourceWriterError::ReplayUncertaintyOutcomeUncertain),
        }
    }

    /// Atomically advance the source-owned replay barrier from an admitted continuity proof.
    ///
    /// The proof is created only by reconciliation admission. The supplied prior must still
    /// equal the checkpoint under the SQLite writer reservation. Duplicate delivery of the
    /// same proof returns the already committed token. No pathless hint or audit can enter here.
    pub fn commit_watcher_replay_checkpoint(
        &self,
        expected: &SourceScanFence,
        prior: &ReplayPriorToken,
        proof: &WatcherContinuityProof,
    ) -> Result<ReplayPriorToken, SourceWriterError> {
        self.validate_scan_fence(expected)?;
        if prior.source_id() != expected.source_id()
            || prior.root_identity() != expected.root_identity()
            || proof.source_id() != expected.source_id()
            || proof.root_identity() != expected.root_identity()
            || proof.backend_stream_identity() != prior.backend_stream_identity()
            || proof.watcher_generation() != prior.watcher_generation()
            || proof.prior_acknowledgement().sequence() != prior.acknowledged_sequence()
        {
            return Err(SourceWriterError::ReplayCheckpointMismatch);
        }
        let encoded =
            encode_replay_checkpoint(proof, expected.revision(), expected.lifecycle_generation())
                .ok_or(SourceWriterError::ReplayCheckpointMismatch)?;
        if encoded.len() > 16 * 1024 {
            return Err(SourceWriterError::InvalidRequest(
                "watcher replay checkpoint exceeds the bounded metadata size",
            ));
        }
        let mut batch = self.database.write_batch()?;
        if !batch.matches_revision(expected.revision())? {
            return Err(SourceWriterError::StaleRevision {
                expected: expected.revision(),
                actual: batch.get_revision()?,
            });
        }
        let stored = batch.coordination_metadata(META_SOURCE_WATCHER_CHECKPOINT)?;
        if batch
            .coordination_metadata(META_SOURCE_WATCHER_UNCERTAINTY)?
            .is_some()
        {
            return Err(SourceWriterError::ReplayCheckpointMismatch);
        }
        if stored.as_deref() != Some(encoded.as_str()) {
            let stored_prior = stored.as_deref().and_then(|value| {
                parse_durable_replay_prior(
                    value,
                    expected.source_id(),
                    expected.root_identity(),
                    WatcherGeneration::new(expected.lifecycle_generation()),
                    prior.watcher_generation(),
                )
            });
            if stored_prior.as_ref() != Some(prior) {
                return Err(SourceWriterError::ReplayCheckpointMismatch);
            }
            batch.set_metadata(META_SOURCE_WATCHER_CHECKPOINT, &encoded)?;
        }
        batch.commit_auxiliary_state()?;
        let readback = self.database.read_durable_replay_prior(
            expected.source_id(),
            expected.root_identity(),
            WatcherGeneration::new(expected.lifecycle_generation()),
            prior.watcher_generation(),
        );
        match readback {
            Ok(Some(token))
                if token.acknowledged_sequence() == proof.replay_coverage().through_sequence() =>
            {
                Ok(token)
            }
            Ok(_) | Err(_) => Err(SourceWriterError::ReplayCheckpointOutcomeUncertain {
                event_id: proof.replay_coverage().through_sequence(),
            }),
        }
    }

    /// Commit one bounded replay manifest delta and its acknowledgement in the same transaction.
    ///
    /// The caller keeps the admission ticket and raw batch until this returns a committed
    /// checkpoint. A stale revision, uncertain marker, or mismatched prior rolls the prepared
    /// manifest batch back, so no replay cursor can overtake its source facts.
    pub fn commit_watcher_replay_manifest_batch(
        &self,
        expected: &SourceScanFence,
        mut batch: SourceWriteBatch<'_>,
        prior: &ReplayPriorToken,
        proof: &WatcherContinuityProof,
    ) -> Result<(ManifestCommitResult, ReplayPriorToken), SourceWriterError> {
        if !std::ptr::eq(batch.owner, &self.database) {
            return Err(SourceWriterError::InvalidRequest(
                "replay batch belongs to another source database",
            ));
        }
        self.validate_scan_fence(expected)?;
        if prior.source_id() != expected.source_id()
            || prior.root_identity() != expected.root_identity()
            || proof.source_id() != expected.source_id()
            || proof.root_identity() != expected.root_identity()
            || proof.backend_stream_identity() != prior.backend_stream_identity()
            || proof.watcher_generation() != prior.watcher_generation()
            || proof.prior_acknowledgement().sequence() != prior.acknowledged_sequence()
        {
            return Err(SourceWriterError::ReplayCheckpointMismatch);
        }
        if !batch.matches_revision(expected.revision())? {
            return Err(SourceWriterError::StaleRevision {
                expected: expected.revision(),
                actual: batch.get_revision()?,
            });
        }
        if batch
            .coordination_metadata(META_SOURCE_WATCHER_UNCERTAINTY)?
            .is_some()
        {
            return Err(SourceWriterError::ReplayCheckpointMismatch);
        }
        let stored = batch.coordination_metadata(META_SOURCE_WATCHER_CHECKPOINT)?;
        let stored_prior = stored.as_deref().and_then(|value| {
            parse_durable_replay_prior(
                value,
                expected.source_id(),
                expected.root_identity(),
                WatcherGeneration::new(expected.lifecycle_generation()),
                prior.watcher_generation(),
            )
        });
        if stored_prior.as_ref() != Some(prior) {
            return Err(SourceWriterError::ReplayCheckpointMismatch);
        }
        let committed_revision = expected
            .revision()
            .checked_add(1)
            .ok_or(SourceWriterError::ReplayCheckpointMismatch)?;
        let encoded =
            encode_replay_checkpoint(proof, committed_revision, expected.lifecycle_generation())
                .ok_or(SourceWriterError::ReplayCheckpointMismatch)?;
        if encoded.len() > 16 * 1024 {
            return Err(SourceWriterError::InvalidRequest(
                "watcher replay checkpoint exceeds the bounded metadata size",
            ));
        }
        batch.set_metadata(META_SOURCE_WATCHER_CHECKPOINT, &encoded)?;
        let manifest = batch.commit_with_bounded_manifest_changes(expected.revision())?;
        if manifest.revision != committed_revision {
            return Err(SourceWriterError::ReplayCheckpointOutcomeUncertain {
                event_id: proof.replay_coverage().through_sequence(),
            });
        }
        match self.database.read_durable_replay_prior(
            expected.source_id(),
            expected.root_identity(),
            WatcherGeneration::new(expected.lifecycle_generation()),
            prior.watcher_generation(),
        ) {
            Ok(Some(token))
                if token.acknowledged_sequence() == proof.replay_coverage().through_sequence() =>
            {
                Ok((manifest, token))
            }
            Ok(_) | Err(_) => Err(SourceWriterError::ReplayCheckpointOutcomeUncertain {
                event_id: proof.replay_coverage().through_sequence(),
            }),
        }
    }

    /// Persist the first replay prior after a complete, revision-matched source audit and a
    /// certain live FSEvents capture with no changed source paths.
    ///
    /// The native stream and filesystem traversal must be finished before this transaction.
    /// Existing current authority is never rewound; a legacy or stale checkpoint can be
    /// replaced only by this complete-audit bootstrap path.
    pub fn commit_bootstrap_replay_checkpoint(
        &self,
        expected: &SourceScanFence,
        audit: &SourceAuditCommit,
        evidence: &BootstrapReplayEvidence,
    ) -> Result<ReplayPriorToken, SourceWriterError> {
        self.validate_scan_fence(expected)?;
        if audit.committed_source_revision() != expected.revision()
            || audit.committed_root_identity() != expected.root_identity()
            || evidence.source_id() != expected.source_id()
            || evidence.root_identity() != expected.root_identity()
            || evidence.lifecycle_generation() != expected.lifecycle_generation()
        {
            return Err(SourceWriterError::ReplayBootstrapMismatch);
        }
        let encoded = encode_bootstrap_checkpoint(evidence, expected.revision())
            .ok_or(SourceWriterError::ReplayBootstrapMismatch)?;
        if encoded.len() > 16 * 1024 {
            return Err(SourceWriterError::InvalidRequest(
                "watcher replay bootstrap exceeds the bounded metadata size",
            ));
        }
        let mut batch = self.database.write_batch()?;
        if !batch.matches_revision(expected.revision())? {
            return Err(SourceWriterError::StaleRevision {
                expected: expected.revision(),
                actual: batch.get_revision()?,
            });
        }
        let stored = batch.get_metadata(super::super::META_SOURCE_WATCHER_CHECKPOINT)?;
        let unresolved = batch.coordination_metadata(META_SOURCE_WATCHER_UNCERTAINTY)?;
        if let Some(stored_marker) = unresolved.as_deref() {
            let marker = DurableReplayUncertainty::decode(stored_marker)
                .ok_or(SourceWriterError::ReplayBootstrapMismatch)?;
            // A previously completed audit at this same revision did not cover evidence retained
            // afterward. Only a new complete audit may retire the unresolved marker.
            if marker.source_id() != expected.source_id()
                || marker.root_identity() != expected.root_identity()
                || marker.source_revision() >= audit.committed_source_revision()
            {
                return Err(SourceWriterError::ReplayBootstrapMismatch);
            }
        }
        if stored.as_deref() != Some(encoded.as_str()) {
            if unresolved.is_none()
                && stored
                    .as_deref()
                    .and_then(|value| {
                        parse_durable_replay_prior(
                            value,
                            expected.source_id(),
                            expected.root_identity(),
                            WatcherGeneration::new(expected.lifecycle_generation()),
                            evidence.watcher_generation(),
                        )
                    })
                    .is_some()
            {
                return Err(SourceWriterError::ReplayBootstrapMismatch);
            }
            batch.set_metadata(super::super::META_SOURCE_WATCHER_CHECKPOINT, &encoded)?;
        }
        if unresolved.is_some() {
            batch.remove_coordination_metadata(META_SOURCE_WATCHER_UNCERTAINTY)?;
        }
        batch.commit_auxiliary_state()?;
        match self.database.read_durable_replay_prior(
            expected.source_id(),
            expected.root_identity(),
            WatcherGeneration::new(expected.lifecycle_generation()),
            evidence.watcher_generation(),
        ) {
            Ok(Some(token)) if token.acknowledged_sequence() == evidence.through_sequence() => {
                Ok(token)
            }
            Ok(_) | Err(_) => Err(SourceWriterError::ReplayBootstrapOutcomeUncertain {
                event_id: evidence.through_sequence(),
            }),
        }
    }

    fn validate_readiness_revision(
        &self,
        expected: &SourceRevision,
    ) -> Result<(), SourceWriterError> {
        let current = self.current_revision()?;
        if expected.source_id != current.source_id
            || expected.root_identity != current.root_identity
            || expected.root_binding_identity != current.root_binding_identity
            || expected.database_identity != current.database_identity
            || expected.lifecycle_generation != current.lifecycle_generation
            || expected.lease_epoch != current.lease_epoch
        {
            return Err(SourceWriterError::StaleFence);
        }
        if expected.revision != current.revision {
            return Err(SourceWriterError::StaleRevision {
                expected: expected.revision,
                actual: current.revision,
            });
        }
        Ok(())
    }

    fn validate_readiness_command(
        &self,
        expected: &SourceRevision,
        target: &ReadinessTarget,
    ) -> Result<(), SourceWriterError> {
        self.validate_readiness_revision(expected)?;
        if target.source_id != self.source_id.as_str() {
            return Err(SourceWriterError::StaleFence);
        }
        Ok(())
    }

    /// Persist at most one bounded batch of current source-owned readiness jobs.
    /// Reconciliation and feature computation must finish before this transaction begins.
    pub fn enqueue_readiness_jobs(
        &mut self,
        expected: &SourceRevision,
        deficits: &[ReadinessDeficit],
        created_at: i64,
    ) -> Result<usize, SourceWriterError> {
        if deficits.len() > SOURCE_READINESS_JOB_BATCH_LIMIT {
            return Err(SourceWriterError::InvalidRequest(
                "readiness job batch exceeds the source writer limit",
            ));
        }
        let input_bytes = deficits.iter().try_fold(0usize, |total, deficit| {
            let target = &deficit.target;
            total
                .checked_add(target.source_id.len())?
                .checked_add(target.scope_id.len())?
                .checked_add(target.relative_path.as_ref().map_or(0, String::len))?
                .checked_add(target.required_version.len())?
                .checked_add(target.content_generation.len())
        });
        if !input_bytes.is_some_and(|bytes| bytes <= MAX_RECORD_BYTES) {
            return Err(SourceWriterError::InvalidRequest(
                "readiness job batch exceeds the source writer byte budget",
            ));
        }
        self.validate_readiness_revision(expected)?;
        if deficits
            .iter()
            .any(|deficit| deficit.target.source_id != self.source_id.as_str())
        {
            return Err(SourceWriterError::StaleFence);
        }
        Ok(self
            .database
            .readiness_store()?
            .persist_deficits(deficits, created_at)?)
    }

    /// Claim one published readiness target under the retained source owner.
    pub fn claim_readiness_target(
        &mut self,
        expected: &SourceRevision,
        target: &ReadinessTarget,
        now: i64,
        lease_duration_seconds: i64,
    ) -> Result<Option<ClaimedReadinessWork>, SourceWriterError> {
        self.validate_readiness_command(expected, target)?;
        Ok(self
            .database
            .readiness_store()?
            .claim(target, now, lease_duration_seconds)?)
    }

    /// Renew an exact claimed readiness lease without changing its claim generation.
    pub fn renew_readiness_lease(
        &mut self,
        expected: &SourceRevision,
        claim: &ClaimedReadinessWork,
        now: i64,
        lease_duration_seconds: i64,
    ) -> Result<ReadinessLeaseRenewalOutcome, SourceWriterError> {
        self.validate_readiness_command(expected, claim.target())?;
        Ok(self
            .database
            .readiness_store()?
            .renew_lease(claim, now, lease_duration_seconds)?)
    }

    /// Publish completion of the exact claimed target through the typed readiness store.
    pub fn complete_readiness_work(
        &mut self,
        expected: &SourceRevision,
        claim: &ClaimedReadinessWork,
        completed_at: i64,
        artifact_ref: Option<&str>,
    ) -> Result<ArtifactPublishOutcome, SourceWriterError> {
        self.validate_readiness_command(expected, claim.target())?;
        let mut store = self.database.readiness_store()?;
        Ok(match artifact_ref {
            Some(artifact_ref) => {
                store.complete_with_artifact_ref(claim, completed_at, artifact_ref)?
            }
            None => store.complete(claim, completed_at)?,
        })
    }

    /// Revalidate the exact feature claim's writer instance and current source revision.
    pub fn validate_feature_artifact_claim(
        &self,
        expected: &SourceRevision,
        claim: &crate::sample_sources::readiness::FeatureArtifactClaim,
    ) -> Result<(), SourceWriterError> {
        if !std::sync::Weak::ptr_eq(&claim.owner, &Arc::downgrade(&self.reference_liveness)) {
            return Err(SourceWriterError::StaleFence);
        }
        self.validate_readiness_command(expected, claim.readiness_claim().target())
    }

    /// Prepare held input from the exact current manifest/claim without data reads or SQL mutation.
    pub fn prepare_feature_input(
        &self,
        expected: &SourceRevision,
        claim: &crate::sample_sources::readiness::FeatureArtifactClaim,
    ) -> Result<crate::sample_sources::readiness::SourceFeatureInput, SourceWriterError> {
        use crate::sample_sources::readiness::{
            FEATURE_INPUT_MAX_BYTES, InputPermit, SourceFeatureInput, SourceFeatureInputError as E,
        };
        self.validate_feature_artifact_claim(expected, claim)?;
        let target = claim.readiness_claim().target();
        let path = target
            .relative_path
            .as_ref()
            .ok_or(SourceWriterError::InvalidRequest("feature path missing"))?;
        let relative = RootRelativePath::try_from_path(std::path::PathBuf::from(path))
            .map_err(|_| SourceWriterError::InvalidRequest("invalid feature path"))?;
        let (size, modified, identity, content): (i64, i64, String, String) = self
            .database
            .connection
            .query_row(
                "SELECT file_size, modified_ns, file_identity, content_hash FROM wav_files
             WHERE path = ?1 AND missing = 0 AND file_identity = ?2 AND content_hash = ?3
             AND length(file_identity) <= 512 AND length(content_hash) = 64",
                params![path, target.scope_id, target.content_generation],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .map_err(map_sql_error)?;
        let size = u64::try_from(size)
            .map_err(|_| SourceWriterError::InvalidRequest("invalid feature size"))?;
        if size > FEATURE_INPUT_MAX_BYTES {
            return Err(SourceWriterError::FeatureInput(E::ByteLimit));
        }
        let permit = InputPermit::acquire()?;
        let fence = self.database.source_publication_fence()?;
        let root = self
            .database
            .source_lease
            .as_ref()
            .ok_or(SourceWriterError::StaleFence)?
            .media_root_binding();
        let held = root
            .inspect_index_file(relative.as_path())
            .map_err(|e| match e {
                super::super::SourceIndexInspectionError::Unsupported => E::UnsupportedPlatform,
                _ => E::Changed,
            })?;
        let input = SourceFeatureInput {
            held,
            root,
            fence,
            owner: claim.owner.clone(),
            identity,
            size,
            modified,
            content,
            _permit: permit,
        };
        input.validate()?;
        Ok(input)
    }

    /// Claim one feature target and bind the work to this exact retained writer instance.
    pub fn claim_feature_artifact(
        &mut self,
        expected: &SourceRevision,
        target: &ReadinessTarget,
        now: i64,
        lease_duration_seconds: i64,
    ) -> Result<Option<crate::sample_sources::readiness::FeatureArtifactClaim>, SourceWriterError>
    {
        if target.scope_kind != crate::sample_sources::readiness::ReadinessScopeKind::File
            || target.stage != crate::sample_sources::readiness::ReadinessStage::AnalysisFeatures
            || target.required_version != "features-v1"
        {
            return Err(SourceWriterError::InvalidRequest(
                "unsupported feature target",
            ));
        }
        let owner = Arc::downgrade(&self.reference_liveness);
        Ok(self
            .claim_readiness_target(expected, target, now, lease_duration_seconds)?
            .map(|claim| crate::sample_sources::readiness::FeatureArtifactClaim { claim, owner }))
    }

    /// Atomically publish an immutable v1 feature payload and exact claimed completion.
    /// Vector computation must finish before this owner command begins.
    pub fn publish_feature_artifact(
        &mut self,
        expected: &SourceRevision,
        claim: &crate::sample_sources::readiness::FeatureArtifactClaim,
        payload: &crate::sample_sources::readiness::FeatureArtifactPayload,
        now: i64,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<crate::sample_sources::readiness::FeatureArtifactPublication, SourceWriterError>
    {
        let started = Instant::now();
        let result = (|| {
            self.validate_feature_artifact_claim(expected, claim)?;
            Ok(self.database.readiness_store()?.publish_feature_artifact(
                claim.readiness_claim(),
                payload,
                now,
                cancel,
            )?)
        })();
        self.feature_publication_metrics
            .record(&result, payload.bytes().len(), started.elapsed());
        result
    }

    /// Copy bounded diagnostic counters for this writer acquisition, without SQL or event traffic.
    /// Counters reset on handoff/reacquisition; they never establish durable or physical evidence.
    pub fn feature_publication_metrics(&self) -> SourceFeaturePublicationMetrics {
        self.feature_publication_metrics
    }

    /// Lookup validated immutable bytes under the current owner and exact content target.
    /// Missing, stale or corrupt bytes are unavailable; this command never marks work ready.
    /// Read-only owner checks never renew an idle lease through the control writer.
    pub fn lookup_feature_artifact(
        &mut self,
        expected: &SourceRevision,
        key: &crate::sample_sources::readiness::FeatureArtifactKey,
    ) -> Result<Option<crate::sample_sources::readiness::FeatureArtifactPayload>, SourceWriterError>
    {
        self.validate_readiness_command(expected, key.target())?;
        Ok(self
            .database
            .readiness_store_read_only()?
            .lookup_feature_artifact(key)?)
    }

    /// Reissue a validated key and bounded payload for an exact current target after restart.
    pub fn lookup_feature_artifact_for_target(
        &mut self,
        expected: &SourceRevision,
        target: &ReadinessTarget,
    ) -> Result<
        Option<(
            crate::sample_sources::readiness::FeatureArtifactKey,
            crate::sample_sources::readiness::FeatureArtifactPayload,
        )>,
        SourceWriterError,
    > {
        self.validate_readiness_command(expected, target)?;
        Ok(self
            .database
            .readiness_store_read_only()?
            .lookup_feature_artifact_for_target(target)?)
    }

    /// Inject a bounded fault into this owner's current feature cache for integration tests.
    /// This accepts no SQL/path/database capability and is absent from production builds.
    #[cfg(any(test, feature = "test-support"))]
    pub fn inject_feature_artifact_fixture_fault(
        &mut self,
        expected: &SourceRevision,
        target: &crate::sample_sources::readiness::ReadinessTarget,
        fault: crate::sample_sources::readiness::FeatureArtifactFixtureFault,
    ) -> Result<(), SourceWriterError> {
        self.validate_readiness_command(expected, target)?;
        Ok(self
            .database
            .readiness_store()?
            .inject_feature_artifact_fixture_fault(target, fault)?)
    }

    /// Admit bounded repair of an exact current source-feature entry under this owner.
    /// Healthy bytes, live claims and ambiguous provenance are retained; no other scope is pruned.
    pub fn admit_feature_artifact_rebuild(
        &mut self,
        expected: &SourceRevision,
        target: &ReadinessTarget,
        now: i64,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<crate::sample_sources::readiness::FeatureArtifactRebuildAdmission, SourceWriterError>
    {
        let started = Instant::now();
        let result = (|| {
            self.validate_readiness_command(expected, target)?;
            if target.source_generation
                != i64::try_from(expected.revision()).map_err(|_| SourceWriterError::StaleFence)?
            {
                return Err(SourceWriterError::StaleFence);
            }
            let fence = self
                .reference_read_fence()
                .map_err(|_| SourceWriterError::StaleFence)?;
            Ok(self
                .database
                .readiness_store()?
                .admit_feature_rebuild(&fence, target, now, cancel)?)
        })();
        self.feature_rebuild_metrics
            .record(&result, started.elapsed());
        result
    }

    /// Observe fixed-size process-local rebuild diagnostics under this current audited owner.
    /// No payload/path history, DB recount, write grant or physical reclamation is represented.
    pub fn feature_rebuild_metrics(
        &self,
        expected: &SourceRevision,
    ) -> Result<FeatureRebuildMetrics, SourceWriterError> {
        self.validate_readiness_revision(expected)?;
        Ok(self.feature_rebuild_metrics)
    }

    /// Record a classified failure for the exact claimed readiness target.
    #[allow(clippy::too_many_arguments)]
    pub fn fail_readiness_work(
        &mut self,
        expected: &SourceRevision,
        claim: &ClaimedReadinessWork,
        classification: ReadinessFailureClassification,
        code: &str,
        reason: &str,
        failed_at: i64,
        retry_policy: ReadinessRetryPolicy,
    ) -> Result<ReadinessFailureOutcome, SourceWriterError> {
        self.validate_readiness_command(expected, claim.target())?;
        Ok(self.database.readiness_store()?.fail(
            claim,
            classification,
            code,
            reason,
            failed_at,
            retry_policy,
        )?)
    }

    /// Return a current claim to pending without consuming its retry allowance.
    pub fn release_readiness_work(
        &mut self,
        expected: &SourceRevision,
        claim: &ClaimedReadinessWork,
        released_at: i64,
    ) -> Result<ReadinessWorkMutationOutcome, SourceWriterError> {
        self.validate_readiness_command(expected, claim.target())?;
        Ok(self
            .database
            .readiness_store()?
            .release(claim, released_at)?)
    }

    /// Return a current claim to pending with a cancellation diagnostic.
    pub fn cancel_readiness_work(
        &mut self,
        expected: &SourceRevision,
        claim: &ClaimedReadinessWork,
        reason: &str,
        cancelled_at: i64,
    ) -> Result<ReadinessWorkMutationOutcome, SourceWriterError> {
        self.validate_readiness_command(expected, claim.target())?;
        Ok(self
            .database
            .readiness_store()?
            .cancel(claim, reason, cancelled_at)?)
    }

    /// Borrow the retained database for catalog paths that have not yet been
    /// migrated to typed source-writer commands. The borrow cannot outlive this
    /// owner and must not be used to construct a second writable connection.
    pub fn catalog_database(&self) -> &SourceDatabase {
        &self.database
    }

    /// Return the database to the catalog's explicit drained-close boundary.
    pub fn into_catalog_database(self) -> SourceDatabase {
        self.database
    }

    /// Retain one existing profile/source-lease-bound database as the sole writer owner.
    pub fn new(
        database: SourceDatabase,
        source_id: SourceId,
        lifecycle_generation: u64,
    ) -> Result<Self, SourceWriterError> {
        let writer = Self::retain(database, source_id, lifecycle_generation)?;
        let current = writer.current_revision()?;
        writer.ensure_stored_source_id(current.source_id())?;
        Ok(writer)
    }

    /// Retain a profile-bound source before its first complete manifest audit.
    ///
    /// This issues scan observations only; it does not route a scan mutation.
    /// Curation and browser snapshots still require [`Self::current_revision`]
    /// and its complete-audit check.
    pub fn new_for_scan(
        database: SourceDatabase,
        source_id: SourceId,
        lifecycle_generation: u64,
    ) -> Result<Self, SourceWriterError> {
        let writer = Self::retain(database, source_id, lifecycle_generation)?;
        let current = writer.current_scan_fence()?;
        writer.ensure_stored_source_id(current.source_id())?;
        Ok(writer)
    }

    fn retain(
        database: SourceDatabase,
        source_id: SourceId,
        lifecycle_generation: u64,
    ) -> Result<Self, SourceWriterError> {
        validate_source_id(&source_id)?;
        let writer = Self {
            feature_publication_metrics: SourceFeaturePublicationMetrics::default(),
            reference_liveness: Arc::new(()),
            feature_rebuild_metrics: FeatureRebuildMetrics::default(),
            feature_maintenance_metrics: FeatureMaintenanceMetrics::default(),
            database,
            source_id,
            lifecycle_generation,
            #[cfg(test)]
            fault: None,
        };
        Ok(writer)
    }

    /// Issue a bounded audited reference read fence from this retained writer instance.
    /// The observation expires on writer handoff/close or revision/ownership changes; it is
    /// not a mutation grant and performs no pathname reopen or lease renewal.
    pub fn reference_read_fence(&self) -> Result<SourceReferenceFence, SourceReferenceFenceError> {
        let publication = self.database.source_publication_fence()?;
        let (root_identity, revision) = publication.read_reference_audit()?;
        let binding = self
            .database
            .database_file_binding
            .as_deref()
            .ok_or(SourceReferenceFenceError::InvalidIdentity)?;
        let lease_epoch = u64::try_from(publication.lease_epoch())
            .map_err(|_| SourceReferenceFenceError::InvalidIdentity)?;
        let root_binding_identity = binding.root().identity();
        let database_identity = binding.identity();
        if self.source_id.as_str().is_empty()
            || self.source_id.as_str().len() > 256
            || root_binding_identity.is_empty()
            || root_binding_identity.len() > 256
            || database_identity.is_empty()
            || database_identity.len() > 256
            || lease_epoch == 0
            || self.lifecycle_generation > i64::MAX as u64
        {
            return Err(SourceReferenceFenceError::InvalidIdentity);
        }
        let observation = SourceRevision {
            source_id: self.source_id.clone(),
            root_identity,
            root_binding_identity: root_binding_identity.to_owned(),
            database_identity: database_identity.to_owned(),
            lifecycle_generation: self.lifecycle_generation,
            lease_epoch,
            revision,
        };
        let fence = SourceReferenceFence::issue(observation, publication, &self.reference_liveness);
        fence.validate()?;
        Ok(fence)
    }

    /// Capture bounded scalar registry metadata. No live registration or effect authority.
    pub fn capture_registry_reference_observation(
        &self,
        registration: Option<&super::super::SourceAuthorityRegistration>,
    ) -> Result<
        super::super::SourceRegistryReferenceObservation,
        super::super::SourceRegistryReferenceError,
    > {
        let before = self.current_scan_fence()?;
        let observation = super::super::source_registry_reference::capture(
            &self.database,
            &before,
            registration,
        )?;
        self.validate_scan_fence(&before)?;
        if let Some(registration) = registration {
            let after = registration.registry_reference_observation(&self.database)?;
            if observation.subject().registration() != Some(&after) {
                return Err(super::super::SourceRegistryReferenceError::ObservationChanged);
            }
        }
        Ok(observation)
    }

    /// Observe a live scan position without asserting completed manifest coverage.
    pub fn current_scan_fence(&self) -> Result<SourceScanFence, SourceWriterError> {
        let profile_guard = self
            .database
            .profile_guard
            .as_ref()
            .ok_or(SourceDbError::ProfileOwnershipRequired)?;
        self.database.validate_profile_owned_open(profile_guard)?;
        let lease = self
            .database
            .source_lease
            .as_deref()
            .ok_or(SourceDbError::ProfileOwnershipRequired)?;
        let live_media_root_identity =
            RootIdentity::from_bytes(lease.media_root_binding().identity().as_bytes().to_vec());
        let binding = self
            .database
            .database_file_binding
            .as_deref()
            .ok_or(SourceDbError::ProfileOwnershipRequired)?;
        let revision = self.database.get_revision()?;
        let lease_epoch = u64::try_from(lease.epoch())
            .ok()
            .filter(|epoch| *epoch > 0)
            .ok_or(SourceWriterError::StaleFence)?;
        let root_binding_identity = binding.root().identity();
        let database_identity = binding.identity();
        if live_media_root_identity.as_bytes().is_empty()
            || live_media_root_identity.as_bytes().len() > MAX_RECORD_BYTES
            || root_binding_identity.len() > MAX_RECORD_BYTES
            || database_identity.len() > MAX_RECORD_BYTES
        {
            return Err(SourceWriterError::InvalidRequest(
                "source revision identity exceeds the bounded record size",
            ));
        }
        Ok(SourceScanFence {
            source_id: self.source_id.clone(),
            root_identity: live_media_root_identity,
            root_binding_identity: root_binding_identity.to_owned(),
            database_identity: database_identity.to_owned(),
            lifecycle_generation: self.lifecycle_generation,
            lease_epoch,
            revision,
        })
    }

    /// Revalidate an earlier scan observation under this retained owner.
    pub fn validate_scan_fence(
        &self,
        expected: &SourceScanFence,
    ) -> Result<SourceScanFence, SourceWriterError> {
        let current = self.current_scan_fence()?;
        if expected.source_id != current.source_id
            || expected.root_identity != current.root_identity
            || expected.root_binding_identity != current.root_binding_identity
            || expected.database_identity != current.database_identity
            || expected.lifecycle_generation != current.lifecycle_generation
            || expected.lease_epoch != current.lease_epoch
        {
            return Err(SourceWriterError::StaleFence);
        }
        if expected.revision != current.revision {
            return Err(SourceWriterError::StaleRevision {
                expected: expected.revision,
                actual: current.revision,
            });
        }
        Ok(current)
    }

    /// Commit one already-prepared bounded scan batch on this retained database.
    /// Filesystem traversal, file reads, and hashing must finish before the
    /// batch is constructed. The catalog holds its lifecycle guard for this
    /// call; the writer revalidates the physical lease and exact revision.
    pub fn commit_scan_manifest_batch(
        &self,
        expected: &SourceScanFence,
        batch: SourceWriteBatch<'_>,
    ) -> Result<ManifestCommitResult, SourceWriterError> {
        if !std::ptr::eq(batch.owner, &self.database) {
            return Err(SourceWriterError::InvalidRequest(
                "scan batch belongs to another source database",
            ));
        }
        self.validate_scan_fence(expected)?;
        Ok(batch.commit_with_bounded_manifest_changes(expected.revision)?)
    }

    /// Commit a prepared pending-rename retention batch for the exact completed scan revision.
    /// The retained source owner checks its lease, binding, and batch origin before publication.
    pub fn commit_scan_pending_rename_completion(
        &self,
        expected: &SourceScanFence,
        batch: SourceWriteBatch<'_>,
    ) -> Result<(), SourceWriterError> {
        if !std::ptr::eq(batch.owner, &self.database) {
            return Err(SourceWriterError::InvalidRequest(
                "rename completion belongs to another source database",
            ));
        }
        self.validate_scan_fence(expected)?;
        if !batch.matches_revision(expected.revision)? {
            return Err(SourceWriterError::StaleRevision {
                expected: expected.revision,
                actual: batch.get_revision()?,
            });
        }
        batch.commit_auxiliary_state()?;
        Ok(())
    }

    /// Allocate private directory-truth staging under the retained source owner.
    /// Staging is revision-neutral; the final audit commit fences its publication.
    pub fn begin_scan_directory_truth_generation(
        &self,
        expected_entry_count: u64,
    ) -> Result<u64, SourceWriterError> {
        self.current_scan_fence()?;
        Ok(self
            .database
            .begin_next_source_directory_truth_generation(expected_entry_count)?)
    }

    /// Stage one bounded batch of directory evidence on the retained database.
    pub fn stage_scan_directory_truth_entries(
        &self,
        generation: u64,
        entries: &[SourceDirectoryEntry],
    ) -> Result<(), SourceWriterError> {
        self.current_scan_fence()?;
        Ok(self
            .database
            .stage_source_directory_truth_entries(generation, entries)?)
    }

    /// Remove only private staging rows from an aborted audit, in bounded batches.
    pub fn discard_scan_directory_truth_generation_batch(
        &self,
        generation: u64,
        requested_limit: usize,
    ) -> Result<SourceDirectoryTruthCleanup, SourceWriterError> {
        self.current_scan_fence()?;
        Ok(self
            .database
            .discard_source_directory_truth_generation_batch(generation, requested_limit)?)
    }

    /// Reclaim inactive directory generations in bounded source-owned steps.
    pub fn cleanup_scan_directory_truth(
        &self,
        requested_limit: usize,
    ) -> Result<SourceDirectoryTruthCleanup, SourceWriterError> {
        self.current_scan_fence()?;
        Ok(self
            .database
            .cleanup_source_directory_truth(requested_limit)?)
    }

    /// Atomically activate complete staged directory truth and its manifest
    /// audit at the owner's exact scan revision. Staging and traversal remain
    /// outside this final bounded transaction.
    pub fn commit_scan_audit_completion(
        &self,
        expected: &SourceScanFence,
        batch: SourceWriteBatch<'_>,
        generation: u64,
        completed_at: i64,
        root_identity: RootIdentity,
        audit_request: Option<&SourceAuditRequest>,
    ) -> Result<(SourceAuditCommit, Vec<SourceManifestEntry>), SourceWriterError> {
        if !std::ptr::eq(batch.owner, &self.database) {
            return Err(SourceWriterError::InvalidRequest(
                "scan audit batch belongs to another source database",
            ));
        }
        self.validate_scan_fence(expected)?;
        Ok(batch.commit_manifest_audit_with_staged_directory_truth(
            generation,
            expected.revision,
            completed_at,
            root_identity,
            audit_request,
        )?)
    }

    /// Return a fresh revision only after revalidating authoritative audit coverage.
    pub fn current_revision(&self) -> Result<SourceRevision, SourceWriterError> {
        let scan = self.current_scan_fence()?;
        let audit = self.database.last_authoritative_audit_metadata()?.ok_or(
            SourceWriterError::InvalidRequest(
                "source writer requires a complete authoritative audit",
            ),
        )?;
        if audit.source_revision != scan.revision {
            return Err(SourceWriterError::InvalidRequest(
                "source revision is not aligned with the authoritative audit",
            ));
        }
        if audit.root_identity != scan.root_identity {
            return Err(SourceWriterError::StaleFence);
        }
        Ok(SourceRevision {
            source_id: scan.source_id,
            root_identity: scan.root_identity,
            root_binding_identity: scan.root_binding_identity,
            database_identity: scan.database_identity,
            lifecycle_generation: scan.lifecycle_generation,
            lease_epoch: scan.lease_epoch,
            revision: scan.revision,
        })
    }

    /// Renew this retained writer's lease before qualifying a catalog mutation.
    ///
    /// Read-only revision and publication checks remain strict and do not renew ownership.
    /// A live writer may have been idle past the durable lease expiry while still holding its
    /// source lock; mutation admission revalidates that authority before reading its revision.
    pub fn current_revision_for_command(&self) -> Result<SourceRevision, SourceWriterError> {
        self.database.validate_profile_owned_mutation()?;
        self.current_revision()
    }

    /// Commit one bounded catalog metadata/collection command on the retained source.
    ///
    /// Catalog requests have a revision fence and one terminal response, rather than a durable
    /// operation-journal command identity. This route keeps their existing retry behavior and
    /// avoids consuming the finite operation-receipt table for every UI metadata edit.
    pub fn commit_catalog_curation_command(
        &self,
        expected: &SourceRevision,
        profile_guard: &WritableProfileGuard,
        updates: &[SourceCurationUpdate],
    ) -> Result<SourceCurationCommitReceipt, SourceWriterError> {
        let current = self.current_revision_for_command()?;
        if expected.source_id != current.source_id
            || expected.root_identity != current.root_identity
            || expected.root_binding_identity != current.root_binding_identity
            || expected.database_identity != current.database_identity
            || expected.lifecycle_generation != current.lifecycle_generation
            || expected.lease_epoch != current.lease_epoch
        {
            return Err(SourceWriterError::StaleFence);
        }
        if expected.revision != current.revision {
            return Err(SourceWriterError::StaleRevision {
                expected: expected.revision,
                actual: current.revision,
            });
        }
        Ok(self.database.commit_curation_with_profile_guard(
            profile_guard,
            expected.revision,
            &expected.root_identity,
            updates,
        )?)
    }

    /// Capture a bounded whole-source tag scope on the retained owner thread.
    ///
    /// SQL has a two-second deadline; use SourceSnapshotOwner to also bound the caller while
    /// an OS identity check stalls. The returned observation is not mutation admission.
    pub fn capture_tag_assignment_selection(
        &self,
        expected: &SourceRevision,
        old_label: &str,
        mutation: SourceTagAssignmentMutation,
    ) -> Result<SourceTagAssignmentSelection, SourceWriterError> {
        self.capture_tag_assignment_selection_with_deadline(
            expected,
            old_label,
            mutation,
            Instant::now() + Duration::from_secs(2),
        )
    }

    pub(super) fn capture_tag_assignment_selection_with_deadline(
        &self,
        expected: &SourceRevision,
        old_label: &str,
        mutation: SourceTagAssignmentMutation,
        deadline: Instant,
    ) -> Result<SourceTagAssignmentSelection, SourceWriterError> {
        if Instant::now() >= deadline {
            return Err(SourceWriterError::SnapshotBudgetExceeded);
        }
        let _progress = SnapshotProgressGuard::install(&self.database.connection, deadline);
        let result = (|| {
            // The authoritative-audit helper owns its own SQLite transaction. Keep full
            // owner checks outside this read transaction, then pin its logical revision.
            let before = self.current_revision()?;
            if expected != &before {
                return Err(SourceWriterError::StaleFence);
            }
            let transaction = self
                .database
                .connection
                .unchecked_transaction()
                .map_err(map_sql_error)?;
            if self.database.get_revision()? != expected.revision() {
                return Err(SourceWriterError::StaleFence);
            }
            let selection =
                tag_assignment_selection::capture(&transaction, before, old_label, mutation)?;
            transaction.commit().map_err(map_sql_error)?;
            if expected != &self.current_revision()? {
                return Err(SourceWriterError::StaleFence);
            }
            Ok(selection)
        })();
        if Instant::now() >= deadline {
            return Err(SourceWriterError::SnapshotBudgetExceeded);
        }
        result
    }

    /// Synchronous worker primitive; the owned worker bounds the caller's response time.
    pub(super) fn read_browser_metadata_page_with_deadline(
        &self,
        expected: &SourceRevision,
        after_cursor: Option<&BrowserMetadataCursor>,
        requested_limit: usize,
        deadline: std::time::Instant,
    ) -> Result<SourceSnapshotPage, SourceWriterError> {
        const MAX_INPUT_BYTES: usize = 16 * 1024 * 1024;
        if Instant::now() >= deadline {
            return Err(SourceWriterError::SnapshotBudgetExceeded);
        }
        if after_cursor.is_some_and(|cursor| cursor.as_str().len() > MAX_INPUT_BYTES) {
            return Err(SourceWriterError::SnapshotBudgetExceeded);
        }
        let _progress = SnapshotProgressGuard::install(&self.database.connection, deadline);
        let before = self.current_revision();
        if std::time::Instant::now() >= deadline {
            return Err(SourceWriterError::SnapshotBudgetExceeded);
        }
        let before = before?;
        if expected != &before
            || after_cursor.is_some_and(|cursor| !cursor.matches_owner_revision(&before))
        {
            return Ok(SourceSnapshotPage::Stale { current: before });
        }
        let mut page = self
            .database
            .browser_metadata_page_bounded(
                expected.revision(),
                after_cursor,
                requested_limit,
                MAX_INPUT_BYTES,
            )
            .map_err(|error| {
                if matches!(error, SourceDbError::SnapshotBudgetExceeded)
                    || std::time::Instant::now() >= deadline
                {
                    SourceWriterError::SnapshotBudgetExceeded
                } else {
                    SourceWriterError::Database(error)
                }
            })?;
        if std::time::Instant::now() >= deadline {
            return Err(SourceWriterError::SnapshotBudgetExceeded);
        }
        let after = self.current_revision();
        if std::time::Instant::now() >= deadline {
            return Err(SourceWriterError::SnapshotBudgetExceeded);
        }
        let after = after?;
        if expected != &after || page.revision != expected.revision() {
            return Ok(SourceSnapshotPage::Stale { current: after });
        }
        let bytes = page.files.iter().try_fold(0usize, |total, file| {
            total
                .checked_add(std::mem::size_of::<BrowserFileMetadata>())?
                .checked_add(file.relative_path.as_os_str().len())?
                .checked_add(
                    file.collections
                        .len()
                        .checked_mul(std::mem::size_of::<SampleCollection>())?,
                )
        });
        if std::time::Instant::now() >= deadline
            || page.files.len() > 64
            || !bytes.is_some_and(|bytes| bytes <= 16 * 1024 * 1024)
        {
            return Err(SourceWriterError::SnapshotBudgetExceeded);
        }
        if let Some(cursor) = &mut page.next_cursor {
            cursor.bind_owner_revision(after.clone());
        }
        Ok(SourceSnapshotPage::FreshAsOf {
            revision: after,
            page,
        })
    }

    /// Close the retained source database and release its existing source lease.
    pub fn close(self) -> Result<(), SourceDbError> {
        self.database.close()
    }

    /// Read one retained source receipt as a historical observation after revalidating the
    /// current owner. The returned revision is never a live request constructor or write grant.
    pub fn read_receipt_observation(
        &self,
        identity: SourceWriterCommandIdentity,
    ) -> Result<Option<SourceWriterReceipt>, SourceWriterError> {
        let current = self.current_revision()?;
        self.ensure_stored_source_id(&current.source_id)?;
        let batch = self.database.write_batch()?;
        let result = match read_receipt_row(batch.curation_transaction(), identity)? {
            Some(row) => Some(Self::decode_receipt_observation(&current, identity, row)?.2),
            None => None,
        };
        batch
            .rollback_curation_prepared()
            .map_err(SourceWriterError::from)?;
        Ok(result)
    }

    /// Apply one exact curation command, or return its durable receipt on replay.
    pub fn commit_curation(
        &mut self,
        request: SourceWriterRequest,
    ) -> Result<SourceWriterReceipt, SourceWriterError> {
        let current = self.current_revision()?;
        if request.expected().source_id != current.source_id.as_str() {
            return Err(SourceWriterError::StaleFence);
        }
        let updates = request.updates()?;
        validate_update_values(&updates)?;
        let normalized_paths = normalize_update_paths(&updates)?;
        #[cfg(test)]
        let configured_fault = self.fault.take();
        let mut batch = self.database.write_batch()?;
        let transaction_result = (|| {
            if let Some(row) = read_receipt_row(batch.curation_transaction(), request.identity())? {
                return match Self::resolve_existing_receipt(
                    &current,
                    &request,
                    &normalized_paths,
                    row,
                )? {
                    Ok(receipt) => Ok(Some(receipt)),
                    Err(error) => Err(error),
                };
            }

            ensure_expected_fence(&current, request.expected())?;
            crate::sqlite_wal::enforce_wal_admission(batch.curation_transaction())
                .map_err(SourceDbError::from)?;
            let next_source_revision = request.expected().revision.checked_add(1).ok_or(
                SourceWriterError::InvalidRequest("source revision would overflow"),
            )?;
            i64::try_from(next_source_revision).map_err(|_| {
                SourceWriterError::InvalidRequest("source revision exceeds SQLite integer range")
            })?;
            let directory_generation = validate_curation_preconditions(
                batch.curation_transaction(),
                request.expected().revision,
                &RootIdentity::from_bytes(request.expected().root_identity.clone()),
                &normalized_paths,
            )?;
            apply_curation_updates(&mut batch, &updates)?;
            SourceDatabase::bump_revision(batch.curation_transaction())?;
            let committed_source_revision = batch.get_revision()?;
            if committed_source_revision != next_source_revision {
                return Err(SourceWriterError::InvalidRequest(
                    "source revision did not advance exactly once",
                ));
            }
            let next_revision_sql = i64::try_from(next_source_revision)
                .map_err(|_| SourceWriterError::InvalidRequest("source revision exceeds SQLite"))?;
            let expected_revision_sql = i64::try_from(request.expected().revision)
                .map_err(|_| SourceWriterError::InvalidRequest("source revision exceeds SQLite"))?;
            let changed = batch
                .curation_transaction()
                .execute(
                    "UPDATE source_directory_generations
                     SET published_source_revision = ?1
                     WHERE status = 'active' AND published_source_revision = ?2",
                    params![next_revision_sql, expected_revision_sql],
                )
                .map_err(map_sql_error)?;
            if changed != 1 {
                return Err(SourceDbError::Unexpected.into());
            }
            let changed = batch
                .curation_transaction()
                .execute(
                    "UPDATE metadata SET value = ?1
                     WHERE key = ?2 AND value = ?3",
                    params![
                        next_source_revision.to_string(),
                        super::super::META_LAST_AUTHORITATIVE_AUDIT_REVISION,
                        request.expected().revision.to_string()
                    ],
                )
                .map_err(map_sql_error)?;
            if changed != 1 {
                return Err(SourceDbError::Unexpected.into());
            }
            let browser_paths = normalized_paths
                .iter()
                .map(PathBuf::from)
                .collect::<Vec<_>>();
            let files =
                browser_metadata_rows_for_paths(batch.curation_transaction(), &browser_paths)?;
            if files.len() != browser_paths.len() {
                return Err(SourceDbError::Unexpected.into());
            }
            let receipt = StoredSourceWriterReceipt::from_parts(
                &request,
                &current,
                directory_generation,
                committed_source_revision,
                &files,
            )?;
            let receipt_bytes = {
                #[cfg(test)]
                if configured_fault == Some(SourceWriterFault::OversizedReceipt) {
                    vec![0u8; MAX_RECORD_BYTES + 1]
                } else if configured_fault == Some(SourceWriterFault::ReceiptAtRecordLimit) {
                    let mut bytes = encode_bounded(&receipt)?;
                    bytes.resize(MAX_RECORD_BYTES, b' ');
                    bytes
                } else {
                    encode_bounded(&receipt)?
                }
                #[cfg(not(test))]
                {
                    encode_bounded(&receipt)?
                }
            };
            ensure_receipt_capacity(
                batch.curation_transaction(),
                request.canonical_bytes().len(),
                receipt_bytes.len(),
            )?;
            insert_receipt(
                batch.curation_transaction(),
                &request,
                &current,
                committed_source_revision,
                &receipt_bytes,
            )?;

            #[cfg(test)]
            if configured_fault == Some(SourceWriterFault::BeforeCommit) {
                return Err(SourceWriterError::Database(SourceDbError::Canceled));
            }
            #[cfg(test)]
            if configured_fault == Some(SourceWriterFault::RollbackUncertain) {
                return Err(SourceWriterError::Database(SourceDbError::Canceled));
            }
            Ok(Some(receipt.into_public(request.identity())?))
        })();

        let receipt = match transaction_result {
            Ok(Some(receipt)) => receipt,
            Ok(None) => unreachable!("source writer transaction returned no receipt"),
            Err(error) => {
                #[cfg(test)]
                if configured_fault == Some(SourceWriterFault::RollbackUncertain) {
                    drop(batch);
                    return Err(SourceWriterError::Uncertain {
                        identity: request.identity(),
                        canonical_request: request.canonical_bytes().to_vec(),
                    });
                }
                return match batch.rollback_curation_prepared() {
                    Ok(()) => Err(error),
                    Err(_) => Err(SourceWriterError::Uncertain {
                        identity: request.identity(),
                        canonical_request: request.canonical_bytes().to_vec(),
                    }),
                };
            }
        };
        if let Err(error) = batch.validate_profile_owned_mutation() {
            return match batch.rollback_curation_prepared() {
                Ok(()) => Err(error.into()),
                Err(_) => Err(SourceWriterError::Uncertain {
                    identity: request.identity(),
                    canonical_request: request.canonical_bytes().to_vec(),
                }),
            };
        }
        match batch.commit_curation_prepared_with_outcome() {
            CurationCommitOutcome::Committed => {}
            CurationCommitOutcome::RolledBack(error) => return Err(error.into()),
            CurationCommitOutcome::Uncertain(_error) => {
                return Err(SourceWriterError::Uncertain {
                    identity: request.identity(),
                    canonical_request: request.canonical_bytes().to_vec(),
                });
            }
        }
        #[cfg(test)]
        if configured_fault == Some(SourceWriterFault::AfterCommit) {
            return Err(SourceWriterError::Uncertain {
                identity: request.identity(),
                canonical_request: request.canonical_bytes().to_vec(),
            });
        }
        Ok(receipt)
    }

    fn resolve_existing_receipt(
        current: &SourceRevision,
        request: &SourceWriterRequest,
        normalized_paths: &[String],
        row: StoredReceiptRow,
    ) -> Result<Result<SourceWriterReceipt, SourceWriterError>, SourceWriterError> {
        if row.request_bytes != request.canonical_bytes() {
            return Ok(Err(SourceWriterError::ConflictingCommand {
                identity: request.identity(),
            }));
        }
        let (persisted_request, persisted_receipt, receipt) =
            Self::decode_receipt_observation(current, request.identity(), row)?;
        if persisted_request.expected != *request.expected()
            || persisted_request.updates != request.updates
            || persisted_receipt.files.len() != normalized_paths.len()
            || persisted_receipt
                .files
                .iter()
                .zip(normalized_paths)
                .any(|(file, path)| file.relative_path != *path)
        {
            return Err(SourceWriterError::MalformedReceipt);
        }
        if persisted_receipt.lifecycle_generation != current.lifecycle_generation
            || persisted_receipt.lease_epoch != current.lease_epoch
            || persisted_receipt.root_identity != current.root_identity.as_bytes()
            || persisted_receipt.root_binding_identity != current.root_binding_identity
            || persisted_receipt.database_identity != current.database_identity
        {
            return Ok(Err(SourceWriterError::SupersededLifecycle {
                identity: request.identity(),
            }));
        }
        Ok(Ok(receipt))
    }

    fn decode_receipt_observation(
        current: &SourceRevision,
        identity: SourceWriterCommandIdentity,
        row: StoredReceiptRow,
    ) -> Result<
        (
            StoredSourceWriterRequest,
            StoredSourceWriterReceipt,
            SourceWriterReceipt,
        ),
        SourceWriterError,
    > {
        let persisted_request: StoredSourceWriterRequest = decode_bounded(&row.request_bytes)?;
        let persisted_receipt: StoredSourceWriterReceipt = decode_bounded(&row.receipt_bytes)?;
        if persisted_request.version != SOURCE_WRITER_CODEC_VERSION
            || persisted_receipt.version != SOURCE_WRITER_CODEC_VERSION
            || persisted_request.operation_id != identity.operation_id
            || persisted_request.participant_id != identity.participant_id
            || persisted_request.command_id != identity.command_id
            || persisted_receipt.operation_id != identity.operation_id
            || persisted_receipt.participant_id != identity.participant_id
            || persisted_receipt.command_id != identity.command_id
            || persisted_request.source_id != current.source_id.as_str()
            || persisted_receipt.source_id != current.source_id.as_str()
            || row.source_id != persisted_receipt.source_id
            || row.root_identity != persisted_receipt.root_identity
            || row.root_binding_identity != persisted_receipt.root_binding_identity
            || row.database_identity != persisted_receipt.database_identity
            || row.lifecycle_generation != persisted_receipt.lifecycle_generation.to_string()
            || row.lease_epoch
                != i64::try_from(persisted_receipt.lease_epoch)
                    .map_err(|_| SourceWriterError::MalformedReceipt)?
            || row.expected_revision
                != i64::try_from(persisted_receipt.expected_revision)
                    .map_err(|_| SourceWriterError::MalformedReceipt)?
            || row.committed_revision
                != i64::try_from(persisted_receipt.committed_revision)
                    .map_err(|_| SourceWriterError::MalformedReceipt)?
        {
            return Err(SourceWriterError::MalformedReceipt);
        }
        let expected_committed_revision = persisted_request
            .expected
            .revision
            .checked_add(1)
            .ok_or(SourceWriterError::MalformedReceipt)?;
        let updates = persisted_request
            .updates
            .iter()
            .map(StoredCurationUpdate::to_live)
            .collect::<Result<Vec<_>, _>>()?;
        validate_update_values(&updates).map_err(SourceWriterError::Database)?;
        let normalized_paths =
            normalize_update_paths(&updates).map_err(SourceWriterError::Database)?;
        if persisted_receipt.expected_revision != persisted_request.expected.revision
            || persisted_receipt.committed_revision != expected_committed_revision
            || persisted_receipt.committed_revision > current.revision
            || persisted_receipt.files.len() != normalized_paths.len()
            || persisted_receipt
                .files
                .iter()
                .zip(&normalized_paths)
                .any(|(file, path)| file.relative_path != *path)
        {
            return Err(SourceWriterError::MalformedReceipt);
        }
        let receipt = persisted_receipt.clone().into_public(identity)?;
        Ok((persisted_request, persisted_receipt, receipt))
    }

    fn ensure_stored_source_id(&self, source_id: &SourceId) -> Result<(), SourceWriterError> {
        let lookahead = SOURCE_WRITER_RECEIPT_LIMIT
            .checked_add(1)
            .ok_or(SourceWriterError::CapacityExceeded)?;
        let mut length_statement = self
            .database
            .connection
            .prepare(
                "SELECT COALESCE(length(CAST(source_id AS BLOB)), -1)
                 FROM source_participant_curation_receipts
                 ORDER BY committed_revision
                 LIMIT ?1",
            )
            .map_err(map_sql_error)?;
        let source_lengths = length_statement
            .query_map(
                [i64::try_from(lookahead).map_err(|_| SourceWriterError::CapacityExceeded)?],
                |row| row.get::<_, i64>(0),
            )
            .map_err(map_sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sql_error)?;
        if source_lengths.len() >= lookahead {
            return Err(SourceWriterError::CapacityExceeded);
        }
        if source_lengths.iter().any(|length| {
            *length <= 0
                || usize::try_from(*length).map_or(true, |length| length > SOURCE_ID_MAX_BYTES)
        }) {
            return Err(SourceWriterError::MalformedReceipt);
        }
        let mut statement = self
            .database
            .connection
            .prepare(
                "SELECT source_id
                 FROM source_participant_curation_receipts
                 ORDER BY committed_revision
                 LIMIT ?1",
            )
            .map_err(map_sql_error)?;
        let source_ids = statement
            .query_map(
                [i64::try_from(lookahead).map_err(|_| SourceWriterError::CapacityExceeded)?],
                |row| row.get::<_, String>(0),
            )
            .map_err(map_sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sql_error)?;
        if source_ids.len() >= lookahead {
            return Err(SourceWriterError::CapacityExceeded);
        }
        if source_ids.iter().any(|stored| stored != source_id.as_str()) {
            return Err(SourceWriterError::StaleFence);
        }
        Ok(())
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn inject_fault_for_tests(&mut self, fault: &'static str) {
        self.fault = match fault {
            "before_commit" => Some(SourceWriterFault::BeforeCommit),
            "oversized_receipt" => Some(SourceWriterFault::OversizedReceipt),
            "receipt_at_record_limit" => Some(SourceWriterFault::ReceiptAtRecordLimit),
            "rollback_uncertain" => Some(SourceWriterFault::RollbackUncertain),
            "after_commit" => Some(SourceWriterFault::AfterCommit),
            _ => None,
        };
    }

    #[cfg(test)]
    pub(crate) fn database_for_tests(&self) -> &SourceDatabase {
        &self.database
    }
}

fn validate_source_id(source_id: &SourceId) -> Result<(), SourceWriterError> {
    if source_id.as_str().is_empty() || source_id.as_str().len() > SOURCE_ID_MAX_BYTES {
        return Err(SourceWriterError::InvalidRequest(
            "source identity is out of bounds",
        ));
    }
    Ok(())
}

fn validate_revision_bounds(revision: &SourceRevision) -> Result<(), SourceWriterError> {
    if revision.root_identity.as_bytes().is_empty()
        || revision.root_identity.as_bytes().len() > MAX_RECORD_BYTES
        || revision.root_binding_identity.len() > MAX_RECORD_BYTES
        || revision.database_identity.len() > MAX_RECORD_BYTES
    {
        return Err(SourceWriterError::InvalidRequest(
            "source revision identity exceeds the bounded record size",
        ));
    }
    Ok(())
}

fn ensure_expected_fence(
    current: &SourceRevision,
    expected: &StoredSourceRevision,
) -> Result<(), SourceWriterError> {
    if expected.source_id != current.source_id.as_str()
        || expected.root_identity != current.root_identity.as_bytes()
        || expected.root_binding_identity != current.root_binding_identity
        || expected.database_identity != current.database_identity
        || expected.lifecycle_generation != current.lifecycle_generation
        || expected.lease_epoch != current.lease_epoch
    {
        return Err(SourceWriterError::StaleFence);
    }
    if expected.revision != current.revision {
        return Err(SourceWriterError::StaleRevision {
            expected: expected.revision,
            actual: current.revision,
        });
    }
    Ok(())
}

fn encode_bounded<T: Serialize>(value: &T) -> Result<Vec<u8>, SourceWriterError> {
    let mut writer = BoundedJsonWriter::new(MAX_RECORD_BYTES);
    let mut serializer = serde_json::Serializer::new(&mut writer);
    value
        .serialize(&mut serializer)
        .map_err(|_| SourceWriterError::InvalidRequest("source writer value cannot be encoded"))?;
    if writer.bytes.is_empty() {
        return Err(SourceWriterError::InvalidRequest(
            "source writer value exceeds the bounded record size",
        ));
    }
    Ok(writer.bytes)
}

struct BoundedJsonWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl BoundedJsonWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(limit.min(4096)),
            limit,
        }
    }
}

impl Write for BoundedJsonWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next_len = self.bytes.len().checked_add(bytes.len()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "bounded JSON length overflow")
        })?;
        if next_len > self.limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bounded JSON value exceeds the record limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn decode_bounded<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, SourceWriterError> {
    if bytes.is_empty() || bytes.len() > MAX_RECORD_BYTES {
        return Err(SourceWriterError::MalformedReceipt);
    }
    serde_json::from_slice(bytes).map_err(|_| SourceWriterError::MalformedReceipt)
}

fn read_receipt_row(
    transaction: &Transaction<'_>,
    identity: SourceWriterCommandIdentity,
) -> Result<Option<StoredReceiptRow>, SourceWriterError> {
    let matching_rows = transaction
        .query_row(
            "SELECT count(*) FROM (
                SELECT 1 FROM source_participant_curation_receipts
                WHERE operation_id = ?1 AND participant_id = ?2 AND command_id = ?3
                LIMIT 2
             )",
            params![
                identity.operation_id.to_string(),
                identity.participant_id.to_string(),
                identity.command_id.to_string()
            ],
            |row| row.get::<_, i64>(0),
        )
        .map_err(map_sql_error)?;
    if matching_rows > 1 {
        return Err(SourceWriterError::MalformedReceipt);
    }
    if matching_rows == 0 {
        return Ok(None);
    }

    let (request_len, receipt_len, source_len, root_len, root_binding_len, database_len) =
        transaction
            .query_row(
                "SELECT request_len, receipt_len,
                    COALESCE(length(CAST(source_id AS BLOB)), -1),
                    COALESCE(length(root_identity), -1),
                    COALESCE(length(CAST(root_binding_identity AS BLOB)), -1),
                    COALESCE(length(CAST(database_identity AS BLOB)), -1)
             FROM source_participant_curation_receipts
             WHERE operation_id = ?1 AND participant_id = ?2 AND command_id = ?3",
                params![
                    identity.operation_id.to_string(),
                    identity.participant_id.to_string(),
                    identity.command_id.to_string()
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                },
            )
            .map_err(map_sql_error)?;
    if source_len <= 0
        || source_len > i64::try_from(SOURCE_ID_MAX_BYTES).unwrap_or(i64::MAX)
        || root_len <= 0
        || root_binding_len <= 0
        || database_len <= 0
        || usize::try_from(source_len).map_or(true, |length| length > SOURCE_ID_MAX_BYTES)
        || usize::try_from(root_len).map_or(true, |length| length > MAX_RECORD_BYTES)
        || usize::try_from(root_binding_len).map_or(true, |length| length > MAX_RECORD_BYTES)
        || usize::try_from(database_len).map_or(true, |length| length > MAX_RECORD_BYTES)
    {
        return Err(SourceWriterError::MalformedReceipt);
    }
    let request_len =
        usize::try_from(request_len).map_err(|_| SourceWriterError::MalformedReceipt)?;
    let receipt_len =
        usize::try_from(receipt_len).map_err(|_| SourceWriterError::MalformedReceipt)?;
    if request_len == 0
        || receipt_len == 0
        || request_len > MAX_RECORD_BYTES
        || receipt_len > MAX_RECORD_BYTES
        || request_len
            .checked_add(receipt_len)
            .is_none_or(|length| length > SOURCE_WRITER_RECEIPT_BYTES_LIMIT)
    {
        return Err(SourceWriterError::MalformedReceipt);
    }
    let actual_lengths = transaction
        .query_row(
            "SELECT COALESCE(length(request_bytes), -1), COALESCE(length(receipt_bytes), -1)
             FROM source_participant_curation_receipts
             WHERE operation_id = ?1 AND participant_id = ?2 AND command_id = ?3",
            params![
                identity.operation_id.to_string(),
                identity.participant_id.to_string(),
                identity.command_id.to_string()
            ],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .map_err(map_sql_error)?;
    if actual_lengths
        != (
            i64::try_from(request_len).map_err(|_| SourceWriterError::MalformedReceipt)?,
            i64::try_from(receipt_len).map_err(|_| SourceWriterError::MalformedReceipt)?,
        )
    {
        return Err(SourceWriterError::MalformedReceipt);
    }
    let row = transaction
        .query_row(
            "SELECT source_id, root_identity, root_binding_identity, database_identity,
                    lifecycle_generation, lease_epoch, expected_revision, committed_revision,
                    request_bytes, receipt_bytes
             FROM source_participant_curation_receipts
             WHERE operation_id = ?1 AND participant_id = ?2 AND command_id = ?3",
            params![
                identity.operation_id.to_string(),
                identity.participant_id.to_string(),
                identity.command_id.to_string()
            ],
            |row| {
                Ok(StoredReceiptRow {
                    source_id: row.get(0)?,
                    root_identity: row.get(1)?,
                    root_binding_identity: row.get(2)?,
                    database_identity: row.get(3)?,
                    lifecycle_generation: row.get(4)?,
                    lease_epoch: row.get(5)?,
                    expected_revision: row.get(6)?,
                    committed_revision: row.get(7)?,
                    request_bytes: row.get(8)?,
                    receipt_bytes: row.get(9)?,
                })
            },
        )
        .map_err(map_sql_error)?;
    if request_len != row.request_bytes.len() || receipt_len != row.receipt_bytes.len() {
        return Err(SourceWriterError::MalformedReceipt);
    }
    Ok(Some(row))
}

fn ensure_receipt_capacity(
    transaction: &Transaction<'_>,
    request_len: usize,
    receipt_len: usize,
) -> Result<(), SourceWriterError> {
    if request_len == 0
        || request_len > MAX_RECORD_BYTES
        || receipt_len == 0
        || receipt_len > MAX_RECORD_BYTES
    {
        return Err(SourceWriterError::CapacityExceeded);
    }
    let mut statement = transaction
        .prepare(
            "SELECT request_len, receipt_len, length(request_bytes), length(receipt_bytes)
             FROM source_participant_curation_receipts
             ORDER BY committed_revision
             LIMIT ?1",
        )
        .map_err(map_sql_error)?;
    let lookahead = SOURCE_WRITER_RECEIPT_LIMIT
        .checked_add(1)
        .ok_or(SourceWriterError::CapacityExceeded)?;
    let rows = statement
        .query_map(
            [i64::try_from(lookahead).map_err(|_| SourceWriterError::CapacityExceeded)?],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            },
        )
        .map_err(map_sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_sql_error)?;
    if rows.len() >= SOURCE_WRITER_RECEIPT_LIMIT {
        return Err(SourceWriterError::CapacityExceeded);
    }
    let existing_bytes = rows.into_iter().try_fold(
        0usize,
        |total, (request, receipt, actual_request, actual_receipt)| {
            let request =
                usize::try_from(request).map_err(|_| SourceWriterError::MalformedReceipt)?;
            let receipt =
                usize::try_from(receipt).map_err(|_| SourceWriterError::MalformedReceipt)?;
            let actual_request = actual_request
                .and_then(|length| usize::try_from(length).ok())
                .ok_or(SourceWriterError::MalformedReceipt)?;
            let actual_receipt = actual_receipt
                .and_then(|length| usize::try_from(length).ok())
                .ok_or(SourceWriterError::MalformedReceipt)?;
            if request == 0
                || request > MAX_RECORD_BYTES
                || receipt == 0
                || receipt > MAX_RECORD_BYTES
                || request != actual_request
                || receipt != actual_receipt
            {
                return Err(SourceWriterError::MalformedReceipt);
            }
            total
                .checked_add(actual_request)
                .and_then(|value| value.checked_add(actual_receipt))
                .ok_or(SourceWriterError::CapacityExceeded)
        },
    )?;
    let combined = existing_bytes
        .checked_add(request_len)
        .and_then(|value| value.checked_add(receipt_len))
        .ok_or(SourceWriterError::CapacityExceeded)?;
    if combined > SOURCE_WRITER_RECEIPT_BYTES_LIMIT {
        return Err(SourceWriterError::CapacityExceeded);
    }
    Ok(())
}

fn insert_receipt(
    transaction: &Transaction<'_>,
    request: &SourceWriterRequest,
    current: &SourceRevision,
    committed_source_revision: u64,
    receipt_bytes: &[u8],
) -> Result<(), SourceWriterError> {
    let request_len = i64::try_from(request.canonical_bytes().len())
        .map_err(|_| SourceWriterError::CapacityExceeded)?;
    let receipt_len =
        i64::try_from(receipt_bytes.len()).map_err(|_| SourceWriterError::CapacityExceeded)?;
    transaction
        .execute(
            "INSERT INTO source_participant_curation_receipts (
                operation_id, participant_id, command_id, source_id, root_identity,
                root_binding_identity, database_identity, lifecycle_generation, lease_epoch, expected_revision,
                committed_revision, request_bytes, receipt_bytes, request_len, receipt_len
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                request.identity.operation_id.to_string(),
                request.identity.participant_id.to_string(),
                request.identity.command_id.to_string(),
                current.source_id.as_str(),
                current.root_identity.as_bytes(),
                current.root_binding_identity,
                current.database_identity,
                current.lifecycle_generation.to_string(),
                i64::try_from(current.lease_epoch)
                    .map_err(|_| SourceWriterError::CapacityExceeded)?,
                i64::try_from(request.expected().revision)
                    .map_err(|_| SourceWriterError::CapacityExceeded)?,
                i64::try_from(committed_source_revision)
                    .map_err(|_| SourceWriterError::CapacityExceeded)?,
                request.canonical_bytes(),
                receipt_bytes,
                request_len,
                receipt_len,
            ],
        )
        .map_err(map_sql_error)?;
    Ok(())
}

#[derive(Clone, Debug)]
struct StoredReceiptRow {
    source_id: String,
    root_identity: Vec<u8>,
    root_binding_identity: String,
    database_identity: String,
    lifecycle_generation: String,
    lease_epoch: i64,
    expected_revision: i64,
    committed_revision: i64,
    request_bytes: Vec<u8>,
    receipt_bytes: Vec<u8>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredSourceWriterRequest {
    version: u8,
    operation_id: OperationId,
    participant_id: ParticipantId,
    command_id: SourceCommandId,
    source_id: String,
    expected: StoredSourceRevision,
    updates: Vec<StoredCurationUpdate>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredSourceRevision {
    source_id: String,
    root_identity: Vec<u8>,
    root_binding_identity: String,
    database_identity: String,
    lifecycle_generation: u64,
    lease_epoch: u64,
    revision: u64,
}

impl StoredSourceRevision {
    fn from_live(value: &SourceRevision) -> Self {
        Self {
            source_id: value.source_id.as_str().to_owned(),
            root_identity: value.root_identity.as_bytes().to_vec(),
            root_binding_identity: value.root_binding_identity.clone(),
            database_identity: value.database_identity.clone(),
            lifecycle_generation: value.lifecycle_generation,
            lease_epoch: value.lease_epoch,
            revision: value.revision,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum StoredCurationUpdate {
    SetLastPlayedAt {
        path: String,
        played_at: Option<i64>,
    },
    SetRating {
        path: String,
        rating: i8,
    },
    SetRatingAndLastCuratedAt {
        path: String,
        rating: i8,
        last_curated_at: i64,
    },
    SetLocked {
        path: String,
        locked: bool,
    },
    AddCollection {
        path: String,
        collection: u8,
    },
    RemoveCollection {
        path: String,
        collection: u8,
    },
    AddTag {
        path: String,
        label: String,
    },
    RemoveTag {
        path: String,
        label: String,
    },
    AssignExistingTag {
        path: String,
        tag_id: i64,
    },
    RemoveExistingTag {
        path: String,
        tag_id: i64,
    },
}

impl StoredCurationUpdate {
    fn path(path: &RootRelativePath) -> Result<String, SourceWriterError> {
        normalize_relative_path(path.as_path()).map_err(SourceWriterError::Database)
    }

    fn from_live(update: &SourceCurationUpdate) -> Result<Self, SourceWriterError> {
        Ok(match update {
            SourceCurationUpdate::SetLastPlayedAt { path, played_at } => Self::SetLastPlayedAt {
                path: Self::path(path)?,
                played_at: *played_at,
            },
            SourceCurationUpdate::SetRating { path, rating } => Self::SetRating {
                path: Self::path(path)?,
                rating: rating.val(),
            },
            SourceCurationUpdate::SetRatingAndLastCuratedAt {
                path,
                rating,
                last_curated_at,
            } => Self::SetRatingAndLastCuratedAt {
                path: Self::path(path)?,
                rating: rating.val(),
                last_curated_at: *last_curated_at,
            },
            SourceCurationUpdate::SetLocked { path, locked } => Self::SetLocked {
                path: Self::path(path)?,
                locked: *locked,
            },
            SourceCurationUpdate::AddCollection { path, collection } => Self::AddCollection {
                path: Self::path(path)?,
                collection: collection.index(),
            },
            SourceCurationUpdate::RemoveCollection { path, collection } => Self::RemoveCollection {
                path: Self::path(path)?,
                collection: collection.index(),
            },
            SourceCurationUpdate::AddTag { path, label } => Self::AddTag {
                path: Self::path(path)?,
                label: label.clone(),
            },
            SourceCurationUpdate::RemoveTag { path, label } => Self::RemoveTag {
                path: Self::path(path)?,
                label: label.clone(),
            },
            SourceCurationUpdate::AssignExistingTag { path, tag_id } => Self::AssignExistingTag {
                path: Self::path(path)?,
                tag_id: *tag_id,
            },
            SourceCurationUpdate::RemoveExistingTag { path, tag_id } => Self::RemoveExistingTag {
                path: Self::path(path)?,
                tag_id: *tag_id,
            },
        })
    }

    fn to_live(&self) -> Result<SourceCurationUpdate, SourceWriterError> {
        let path = |path: &str| {
            RootRelativePath::try_from_path(PathBuf::from(path))
                .map_err(|_| SourceWriterError::MalformedReceipt)
        };
        Ok(match self {
            Self::SetLastPlayedAt {
                path: value,
                played_at,
            } => SourceCurationUpdate::SetLastPlayedAt {
                path: path(value)?,
                played_at: *played_at,
            },
            Self::SetRating {
                path: value,
                rating,
            } => SourceCurationUpdate::SetRating {
                path: path(value)?,
                rating: Rating::new(*rating),
            },
            Self::SetRatingAndLastCuratedAt {
                path: value,
                rating,
                last_curated_at,
            } => SourceCurationUpdate::SetRatingAndLastCuratedAt {
                path: path(value)?,
                rating: Rating::new(*rating),
                last_curated_at: *last_curated_at,
            },
            Self::SetLocked {
                path: value,
                locked,
            } => SourceCurationUpdate::SetLocked {
                path: path(value)?,
                locked: *locked,
            },
            Self::AddCollection {
                path: value,
                collection,
            } => SourceCurationUpdate::AddCollection {
                path: path(value)?,
                collection: SampleCollection::new(*collection)
                    .ok_or(SourceWriterError::MalformedReceipt)?,
            },
            Self::RemoveCollection {
                path: value,
                collection,
            } => SourceCurationUpdate::RemoveCollection {
                path: path(value)?,
                collection: SampleCollection::new(*collection)
                    .ok_or(SourceWriterError::MalformedReceipt)?,
            },
            Self::AddTag { path: value, label } => SourceCurationUpdate::AddTag {
                path: path(value)?,
                label: label.clone(),
            },
            Self::RemoveTag { path: value, label } => SourceCurationUpdate::RemoveTag {
                path: path(value)?,
                label: label.clone(),
            },
            Self::AssignExistingTag {
                path: value,
                tag_id,
            } => SourceCurationUpdate::AssignExistingTag {
                path: path(value)?,
                tag_id: *tag_id,
            },
            Self::RemoveExistingTag {
                path: value,
                tag_id,
            } => SourceCurationUpdate::RemoveExistingTag {
                path: path(value)?,
                tag_id: *tag_id,
            },
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredSourceWriterReceipt {
    version: u8,
    operation_id: OperationId,
    participant_id: ParticipantId,
    command_id: SourceCommandId,
    source_id: String,
    root_identity: Vec<u8>,
    root_binding_identity: String,
    database_identity: String,
    lifecycle_generation: u64,
    lease_epoch: u64,
    expected_revision: u64,
    committed_revision: u64,
    directory_generation: u64,
    files: Vec<StoredBrowserFileMetadata>,
}

impl StoredSourceWriterReceipt {
    fn from_parts(
        request: &SourceWriterRequest,
        current: &SourceRevision,
        directory_generation: u64,
        committed_revision: u64,
        files: &[BrowserFileMetadata],
    ) -> Result<Self, SourceWriterError> {
        let files = files
            .iter()
            .map(StoredBrowserFileMetadata::from_live)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            version: SOURCE_WRITER_CODEC_VERSION,
            operation_id: request.identity.operation_id,
            participant_id: request.identity.participant_id,
            command_id: request.identity.command_id,
            source_id: current.source_id.as_str().to_owned(),
            root_identity: current.root_identity.as_bytes().to_vec(),
            root_binding_identity: current.root_binding_identity.clone(),
            database_identity: current.database_identity.clone(),
            lifecycle_generation: current.lifecycle_generation,
            lease_epoch: current.lease_epoch,
            expected_revision: request.expected().revision,
            committed_revision,
            directory_generation,
            files,
        })
    }

    fn into_public(
        self,
        identity: SourceWriterCommandIdentity,
    ) -> Result<SourceWriterReceipt, SourceWriterError> {
        if self.version != SOURCE_WRITER_CODEC_VERSION
            || self.source_id.is_empty()
            || self.root_identity.is_empty()
            || self.root_binding_identity.is_empty()
            || self.database_identity.is_empty()
            || self.committed_revision == 0
            || self.expected_revision >= self.committed_revision
            || self.lease_epoch == 0
            || self.directory_generation == 0
            || self.files.is_empty()
            || self.files.len() > SOURCE_CURATION_BATCH_LIMIT
        {
            return Err(SourceWriterError::MalformedReceipt);
        }
        let files = self
            .files
            .into_iter()
            .map(StoredBrowserFileMetadata::into_live)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(SourceWriterReceipt {
            identity,
            revision: SourceRevision {
                source_id: SourceId::from_string(self.source_id),
                root_identity: RootIdentity::from_bytes(self.root_identity.clone()),
                root_binding_identity: self.root_binding_identity.clone(),
                database_identity: self.database_identity.clone(),
                lifecycle_generation: self.lifecycle_generation,
                lease_epoch: self.lease_epoch,
                revision: self.committed_revision,
            },
            curation: SourceCurationCommitReceipt::from_parts(
                self.committed_revision,
                RootIdentity::from_bytes(self.root_identity),
                self.directory_generation,
                files,
            ),
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredBrowserFileMetadata {
    relative_path: String,
    file_size: u64,
    modified_ns: i64,
    missing: bool,
    rating: i8,
    locked: bool,
    collections: Vec<u8>,
    last_played_at: Option<i64>,
    last_curated_at: Option<i64>,
    playback_type: Option<SourcePlaybackType>,
}

impl StoredBrowserFileMetadata {
    fn from_live(file: &BrowserFileMetadata) -> Result<Self, SourceWriterError> {
        let relative_path = file
            .relative_path
            .to_str()
            .ok_or(SourceWriterError::InvalidRequest(
                "receipt path is not valid Unicode",
            ))?
            .to_owned();
        Ok(Self {
            relative_path,
            file_size: file.file_size,
            modified_ns: file.modified_ns,
            missing: file.missing,
            rating: file.rating.val(),
            locked: file.locked,
            collections: file
                .collections
                .iter()
                .map(|collection| collection.index())
                .collect(),
            last_played_at: file.last_played_at,
            last_curated_at: file.last_curated_at,
            playback_type: file.playback_type,
        })
    }

    fn into_live(self) -> Result<BrowserFileMetadata, SourceWriterError> {
        if !(-3..=3).contains(&self.rating)
            || self
                .collections
                .windows(2)
                .any(|window| window[0] >= window[1])
            || self
                .collections
                .iter()
                .any(|value| *value as usize >= SampleCollection::COUNT)
        {
            return Err(SourceWriterError::MalformedReceipt);
        }
        let collections = self
            .collections
            .into_iter()
            .map(|value| SampleCollection::new(value).ok_or(SourceWriterError::MalformedReceipt))
            .collect::<Result<Vec<_>, _>>()?;
        let relative_path = RootRelativePath::try_from_path(PathBuf::from(self.relative_path))
            .map_err(|_| SourceWriterError::MalformedReceipt)?
            .into_path();
        Ok(BrowserFileMetadata {
            relative_path,
            file_size: self.file_size,
            modified_ns: self.modified_ns,
            missing: self.missing,
            rating: Rating::new(self.rating),
            locked: self.locked,
            collections,
            last_played_at: self.last_played_at,
            last_curated_at: self.last_curated_at,
            playback_type: self.playback_type,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_command_id_is_stable_and_not_a_live_fence() {
        let uuid = Uuid::from_u128(7);
        assert_eq!(SourceCommandId::from_uuid(uuid).as_uuid(), uuid);
    }

    #[test]
    fn stored_update_roundtrips_all_variants() {
        let path = RootRelativePath::try_from_path(PathBuf::from("one.wav")).unwrap();
        let updates = vec![
            SourceCurationUpdate::SetLastPlayedAt {
                path: path.clone(),
                played_at: Some(1),
            },
            SourceCurationUpdate::SetRating {
                path: path.clone(),
                rating: Rating::KEEP_3,
            },
            SourceCurationUpdate::SetRatingAndLastCuratedAt {
                path: path.clone(),
                rating: Rating::TRASH_1,
                last_curated_at: 2,
            },
            SourceCurationUpdate::SetLocked {
                path: path.clone(),
                locked: true,
            },
            SourceCurationUpdate::AddCollection {
                path: path.clone(),
                collection: SampleCollection::new(1).unwrap(),
            },
            SourceCurationUpdate::RemoveCollection {
                path: path.clone(),
                collection: SampleCollection::new(2).unwrap(),
            },
            SourceCurationUpdate::AddTag {
                path: path.clone(),
                label: "tag".to_owned(),
            },
            SourceCurationUpdate::RemoveTag {
                path: path.clone(),
                label: "tag".to_owned(),
            },
            SourceCurationUpdate::AssignExistingTag {
                path: path.clone(),
                tag_id: 3,
            },
            SourceCurationUpdate::RemoveExistingTag { path, tag_id: 3 },
        ];
        let stored = updates
            .iter()
            .map(StoredCurationUpdate::from_live)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let roundtripped = stored
            .iter()
            .map(StoredCurationUpdate::to_live)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(roundtripped, updates);
    }
}
