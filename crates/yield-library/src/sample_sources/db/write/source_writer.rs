//! Source-owned revision-fenced curation commands.
//!
//! This module is deliberately narrower than the operation journal.  A `SourceWriter` owns one
//! already-authorized [`SourceDatabase`] and records source-participant receipts in that same
//! database transaction.  The live revision/fence values below are observations of the retained
//! owner; their persisted DTOs never construct or reacquire an authority.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rusqlite::{Transaction, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::operation_journal::{MAX_RECORD_BYTES, OperationId, ParticipantId};
use crate::sample_sources::SourceId;
use crate::sample_sources::db::read::browser_metadata_rows_for_paths;
use crate::sample_sources::reconciliation::{RootIdentity, RootRelativePath};

use super::super::util::{map_sql_error, normalize_relative_path};
use super::super::{
    BrowserFileMetadata, BrowserMetadataCursor, BrowserMetadataPage, Rating, SampleCollection,
    SourceDatabase, SourceDbError, SourcePlaybackType,
};
use super::curation::{
    SOURCE_CURATION_BATCH_LIMIT, SourceCurationCommitReceipt, SourceCurationUpdate,
    apply_curation_updates, normalize_update_paths, validate_curation_preconditions,
    validate_update_values,
};
use super::transaction::CurationCommitOutcome;

/// Maximum durable source-participant receipts retained by one source database.
pub const SOURCE_WRITER_RECEIPT_LIMIT: usize = 256;
/// Maximum combined request and receipt bytes retained by one source database.
pub const SOURCE_WRITER_RECEIPT_BYTES_LIMIT: usize = 16 * 1024 * 1024;
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

enum SnapshotCommand {
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
    requests: Option<SyncSender<SnapshotCommand>>,
    worker: Option<JoinHandle<SourceWriter>>,
}

impl SourceSnapshotOwner {
    /// Transfer the existing source writer to a single snapshot worker.
    pub fn start(writer: SourceWriter) -> io::Result<Self> {
        let (requests, receiver) = mpsc::sync_channel::<SnapshotCommand>(1);
        let worker = thread::Builder::new()
            .name("yield-source-snapshot".to_owned())
            .spawn(move || {
                while let Ok(command) = receiver.recv() {
                    match command {
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
        })
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
    /// Existing source-database validation or transaction failure.
    #[error(transparent)]
    Database(#[from] SourceDbError),
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
            database,
            source_id,
            lifecycle_generation,
            #[cfg(test)]
            fault: None,
        };
        Ok(writer)
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

    /// Synchronous worker primitive; the owned worker bounds the caller's response time.
    pub(super) fn read_browser_metadata_page_with_deadline(
        &self,
        expected: &SourceRevision,
        after_cursor: Option<&BrowserMetadataCursor>,
        requested_limit: usize,
        deadline: std::time::Instant,
    ) -> Result<SourceSnapshotPage, SourceWriterError> {
        const MAX_INPUT_BYTES: usize = 16 * 1024 * 1024;
        if after_cursor.is_some_and(|cursor| cursor.as_str().len() > MAX_INPUT_BYTES) {
            return Err(SourceWriterError::SnapshotBudgetExceeded);
        }
        let _progress = SnapshotProgressGuard::install(&self.database.connection, deadline);
        let before = self.current_revision();
        if std::time::Instant::now() >= deadline {
            return Err(SourceWriterError::SnapshotBudgetExceeded);
        }
        let before = before?;
        if expected != &before {
            return Ok(SourceSnapshotPage::Stale { current: before });
        }
        let page = self
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
