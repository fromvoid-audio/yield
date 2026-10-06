//! Profile-owned SQLite storage for durable operation intents.
//!
//! This store is deliberately smaller than either the source catalog or the global library
//! database. It owns bounded operation records and staging checkpoints under monotonic
//! journal-observation sequences. A successful insert proves that these bytes reached this SQLite database; it
//! does not admit the operation to filesystem execution or grant any other capability. The store
//! uses one exclusive connection, DELETE rollback journaling, and synchronous EXTRA (the
//! strongest rollback-journal mode) and flushes the already-owned profile/child directories on
//! Unix before returning from open. Windows relies on the bundled SQLite VFS for committed file
//! content and deliberately makes no independent directory-entry power-loss guarantee.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::limits::Limit;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use thiserror::Error;
use uuid::Uuid;

use crate::app_dirs::{ProfileOwnershipError, WritableProfileGuard};
use crate::durable_capacity::VolumeIdentity;
use crate::filesystem_identity::stable_filesystem_identity_from_open_file;
use crate::sample_sources::db::BoundDatabaseRoot;
#[cfg(all(test, target_os = "macos"))]
use crate::sample_sources::db::SourceAuthorityUseProof;
use crate::sample_sources::db::sqlite_identity::{self, DatabaseIdentityError};

use super::admission_claims::{
    AdmissionClaimId, AdmissionClaimInventory, AdmissionClaimValidationError,
};
use super::capacity_provider::{
    CapacityProvider, CapacityProviderError, CapacityRebuildReadiness, ProvisionalCapacityLease,
};
use super::capacity_reserve_namespace::{
    BoundReserve, OwnedReserveAllocation, PhysicalReserveProof,
};
use super::capacity_volume_owner::{VolumeOwnershipCapability, VolumeOwnershipError};
use super::codec::{DecodeError, EncodeError, StoredOperationRecord, decode, encode};
use super::coordinator::{
    CopyIntentArgs, RetryObservation, UniversalCommandArgs, WaveformEditIntentArgs,
};
use super::external_commands::ExternalReconciliationIntentArgs;
use super::full_capacity::{FullCapacityAllocationClass, FullCapacityPlan};
use super::history_commands::HistoryUndoRedoIntentArgs;
use super::model::UniversalIntent;
use super::model::{
    CancellationRequestObservation, MAX_RECORD_BYTES, OperationActor, OperationDisposition,
    OperationId, OperationIntent, OperationKind, OperationPhase, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5,
    SCHEMA_V6, SCHEMA_V7, SCHEMA_V8, SCHEMA_V9, SCHEMA_V10, SCHEMA_V11, SCHEMA_V12, SCHEMA_V13,
    SCHEMA_V14, SCHEMA_V15, SCHEMA_V16, SCHEMA_V17, SCHEMA_V18,
};
use super::namespace_commands::NamespaceCommandArgs;
use super::owner::ChunkUnusedReleaseAuthorization;
use super::removal_commands::{RemovalCommandArgs, StageRetainedFolderIntentArgs, TrashIntentArgs};
use super::render_commands::{
    RenderCommandArgs, WaveformDuplicateDoubleIntentArgs, WaveformNormalizeIntentArgs,
};
use super::retained_resolution_commands::{
    RetainedPurgeIntentArgs, RetainedResolutionCommandArgs, RetainedRestoreIntentArgs,
};
use super::selection_normalize_commands::SelectionNormalizeIntentArgs;
use super::selection_output_commands::{
    ExtractAndTrimIntentArgs, SelectionExtractIntentArgs, SelectionOutputCommandArgs,
};
use super::validation::{RecordConsistency, classify_record};

mod admission_rebuild_backing;
mod chunk_claims;
mod chunk_staging;
pub(super) use admission_rebuild_backing::FundedSnapshot;
mod historical_claim_observation;
mod legacy_allocation_observation;
mod recording_chunks;
mod recording_prestart_budget;
mod recording_prestart_database_claims;
pub use legacy_allocation_observation::{LegacyJournalObservation, LegacyJournalObservationError};
pub(crate) use recording_prestart_budget::COMMAND_INPUT_BYTES as RECORDING_PRESTART_INPUT_BYTES;
mod recording_controls;
pub(crate) use recording_chunks::{RecordingChunkPage, RecordingChunkReceipt};
mod frozen_copy;
#[allow(dead_code)]
mod full_progress;
#[cfg(test)]
pub(crate) mod source_layout_tests;
pub(crate) use frozen_copy::FrozenCopyData;
pub(super) mod current_reserve_custody;
mod database_attempt_metadata;
mod database_attempts;
#[allow(dead_code)] // Genuine preparation/latch consumer is OPT-2136; rows confer no authority.
mod database_claims;
#[allow(dead_code)]
mod ordinary_claims;
mod recovery_reserve;
mod recovery_reserve_binding;
use current_reserve_custody::{BoundCustody, OwnedCustody};
#[cfg(test)]
use current_reserve_custody::{CurrentReserveBoundary, CurrentReserveHook};
mod publication_bindings;
mod staging_checkpoints;
pub(crate) use chunk_claims::{
    ChunkClaimRecord, ChunkClaimRequest, ChunkClaimState, ChunkId, ChunkIssueReceipt,
    ChunkWriteIdentity,
};
pub(crate) use chunk_staging::ChunkStagingReceipt;
pub(crate) use full_progress::FullProgressWriteOutcome;
#[cfg(test)]
pub(crate) use ordinary_claims::MAX_PAGE_ROWS;
pub(crate) use ordinary_claims::{OrdinaryClaimCursor, OrdinaryClaimPage, OrdinaryClaimReadError};
pub(crate) use publication_bindings::{PublicationBindingCapture, PublicationBindingDraft};
pub(crate) use staging_checkpoints::StagingCheckpointAck;
/// Read-only, bounded recovery paging for the profile-owned operation journal.
pub mod recovery;

#[cfg(test)]
mod process_tests;

/// The filename of the profile-local operation journal database.
pub const JOURNAL_DATABASE_FILE_NAME: &str = "journal.sqlite3";
/// The profile child directory that owns the operation journal database.
pub const JOURNAL_DATABASE_ROOT_NAME: &str = "operation_journal";
/// The SQLite application id reserved for the operation journal.
pub const JOURNAL_APPLICATION_ID: u32 = u32::from_be_bytes(*b"WCJ1");
/// The SQLite user-version written by this journal schema.
pub const JOURNAL_SCHEMA_VERSION: u32 = 13;

const SQLITE_HEADER_BYTES: usize = 100;
const SQLITE_HEADER_MAGIC: &[u8; 16] = b"SQLite format 3\0";
const SQLITE_USER_VERSION_OFFSET: usize = 60;
const SQLITE_APPLICATION_ID_OFFSET: usize = 68;
const SQLITE_ROLLBACK_HEADER_BYTES: usize = 28;
const SQLITE_JOURNAL_SUFFIX: &str = "-journal";
const SQLITE_WAL_SUFFIX: &str = "-wal";
const SQLITE_SHM_SUFFIX: &str = "-shm";
const JOURNAL_TABLE_NAME: &str = "operation_journal";
const FULL_PROGRESS_TABLE_NAME: &str = "operation_journal_full_progress";
const LEGACY_JOURNAL_SCHEMA_VERSION: u32 = 1;
const PREVIOUS_JOURNAL_SCHEMA_VERSION: u32 = 2;
const RECOVERY_RESERVE_SCHEMA_VERSION: u32 = 3;
const RECOVERY_RESERVE_BINDING_SCHEMA_VERSION: u32 = 4;
const ORDINARY_CLAIM_SCHEMA_VERSION: u32 = 5;
const ORDINARY_CLAIM_RELEASE_SCHEMA_VERSION: u32 = 6;
const CHUNK_CLAIM_SCHEMA_VERSION: u32 = 7;
const STAGING_CHECKPOINT_SCHEMA_VERSION: u32 = 8;
const CHUNK_STAGING_SCHEMA_VERSION: u32 = 9;
const RECORDING_CHUNKS_SCHEMA_VERSION: u32 = 10;
const DATABASE_CLAIMS_SCHEMA_VERSION: u32 = 11;
const DATABASE_ATTEMPTS_SCHEMA_VERSION: u32 = 12;
const MAX_SCHEMA_OBJECTS: i64 = 16;
const MAX_SCHEMA_COLUMNS: usize = 16;
const MAX_SCHEMA_INDEXES: usize = 8;
#[cfg(test)]
thread_local! {
    // Private diagnostic only: actual bytes copied by the point-row callback.
    static POINT_ROW_MATERIALIZED_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

const MAX_REBUILD_DIAGNOSTICS: usize = 32;
const MAX_REBUILD_DIAGNOSTIC_CHARS: usize = 256;
// Capped diagnostic graph plus the generic prepared consumer's four clones,
// join buffer, and final 512-character reason, all overlapping. UTF-8 uses
// at most four bytes per char and String/Vec growth at most twice the payload.
pub(super) const HISTORICAL_DIAGNOSTIC_BYTES: usize = 2
    * MAX_REBUILD_DIAGNOSTICS
    * (std::mem::size_of::<String>() + 4 * MAX_REBUILD_DIAGNOSTIC_CHARS)
    + 2 * 4 * (std::mem::size_of::<String>() + 4 * MAX_REBUILD_DIAGNOSTIC_CHARS)
    + 2 * (4 * 4 * MAX_REBUILD_DIAGNOSTIC_CHARS + 3 * 2)
    + 2 * 4 * 512
    + 3 * std::mem::size_of::<Vec<String>>();
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
// Keep SQLite itself from materializing an arbitrarily large key/value before the recovery
// scanner can inspect its SQL type and length. This leaves room for the bounded JSON envelope
// while remaining close to the persisted record limit.
const SQLITE_VALUE_LIMIT: i32 = (super::database_attempts::MAX_START_BYTES + 64 * 1024) as i32;

const CREATE_JOURNAL_TABLE: &str = "CREATE TABLE operation_journal (
    operation_id TEXT NOT NULL PRIMARY KEY,
    sequence INTEGER NOT NULL UNIQUE,
    record BLOB NOT NULL CHECK(length(record) <= 1048576)
)";
const CREATE_FULL_PROGRESS_TABLE: &str = "CREATE TABLE operation_journal_full_progress (
    operation_id TEXT NOT NULL PRIMARY KEY,
    sequence INTEGER NOT NULL UNIQUE,
    record BLOB NOT NULL CHECK(length(record) <= 1048576)
)";

/// Errors returned by the profile-owned operation journal store.
#[derive(Debug, Error)]
pub enum OperationJournalStoreError {
    /// Complete ordinary claim/release backing cannot fit the original queued pool.
    #[error("ordinary claim rebuild resource envelope exceeded")]
    OrdinaryRebuildResourceBound,
    /// Historical claim observation cannot fit the original queued command's pool.
    #[error("historical claim observation resource envelope exceeded")]
    HistoricalClaimResourceBound,
    /// The borrowed historical record is retained for audit without granting authority.
    #[error("historical claim observation rejected: {reason}")]
    HistoricalClaimObservation {
        /// Bounded static rejection context.
        reason: String,
    },
    /// Complete retained inventory cannot fit the narrow pre-start resource envelope.
    #[error("recording pre-start resource envelope exceeded")]
    RecordingPrestartResourceBound,
    /// Retained recording facts are invalid, conflicting, stale or over budget.
    #[error("recording retention rejected: {reason}")]
    RecordingRetention {
        /// Bounded rejection context; bytes remain retained for audit.
        reason: String,
    },
    /// The caller did not hold the profile ownership capability, or that capability was replaced.
    #[error(transparent)]
    ProfileOwnership(#[from] ProfileOwnershipError),
    /// SQLite rejected a store operation.
    #[error("operation journal SQLite error: {0}")]
    Sql(#[from] rusqlite::Error),
    /// A no-follow filesystem operation failed while opening or validating the store.
    #[error("operation journal filesystem operation failed at {path}: {source}")]
    Io {
        /// Path involved in the failed operation.
        path: PathBuf,
        /// Underlying filesystem error.
        source: io::Error,
    },
    /// Existing journal bytes or schema require an audit and were left unchanged.
    #[error("operation journal at {path} requires audit: {reason}")]
    RequiresAudit {
        /// Journal path whose contents were retained.
        path: PathBuf,
        /// Bounded reason for refusing the database.
        reason: String,
    },
    /// The platform cannot provide a verified SQLite connection-file identity check.
    #[error("operation journal cannot verify SQLite connection identity: {reason}")]
    IdentityCheckUnavailable {
        /// Platform or SQLite reason for the fail-closed result.
        reason: String,
    },
    /// The database pathname no longer names the file opened by this store.
    #[error("operation journal database was replaced: {path}")]
    DatabaseReplaced {
        /// Database pathname whose identity changed.
        path: PathBuf,
    },
    /// A SQLite file-control identity check reported an unexpected result.
    #[error("SQLite connection identity check failed for {path}: {reason}")]
    ConnectionIdentityMismatch {
        /// Database path being checked.
        path: PathBuf,
        /// Bounded diagnostic from SQLite or the native identity check.
        reason: String,
    },
    /// The requested operation record was retained but could not be classified as safe data.
    #[error("retained operation journal record {operation_id:?} requires audit: {reason}")]
    RetainedRecord {
        /// Operation id read from the lookup key, when it was parseable.
        operation_id: Option<OperationId>,
        /// Bounded decode or consistency diagnostic.
        reason: String,
    },
    /// A caller attempted to insert an operation that is not at the durable-intent phase.
    #[error("operation journal insert requires IntentDurable phase, found {phase:?}")]
    NotIntentDurable {
        /// Phase supplied by the caller.
        phase: OperationPhase,
    },
    /// A durable intent must carry the capacity plan that was used for admission accounting.
    #[error("operation journal durable intent requires a capacity plan")]
    MissingCapacityPlan,
    /// An intent carried evidence reserved for later execution phases.
    #[error("operation journal durable intent carries execution evidence: {kind}")]
    IntentCarriesExecutionEvidence {
        /// Evidence field that was unexpectedly present.
        kind: &'static str,
    },
    /// A record failed the pure stored-record consistency checks.
    #[error("operation journal record is internally inconsistent: {reason}")]
    InconsistentRecord {
        /// Bounded consistency diagnostic.
        reason: String,
    },
    /// The operation id already exists and was not overwritten.
    #[error("operation journal operation id already exists: {operation_id}")]
    DuplicateOperation {
        /// Colliding operation identity.
        operation_id: OperationId,
    },
    /// A sequence value could not be allocated without exceeding SQLite's integer range.
    #[error("operation journal sequence is exhausted")]
    SequenceExhausted,
    /// A cancellation request must carry a positive recovery sequence expectation.
    #[error(
        "operation journal cancellation request requires a positive expected sequence, found {expected}"
    )]
    InvalidExpectedSequence {
        /// Sequence supplied by the caller.
        expected: i64,
    },
    /// A chunk claim row or transition failed closed and retained its durable evidence.
    #[error("operation journal chunk claim error: {reason}")]
    ChunkClaim {
        /// Bounded chunk claim diagnostic.
        reason: String,
    },
    /// A staging checkpoint could not be acknowledged or conflicts with retained evidence.
    #[error("operation journal staging checkpoint error: {reason}")]
    StagingCheckpoint {
        /// Bounded checkpoint diagnostic.
        reason: String,
    },
    /// A staging checkpoint or grant write has no confirmed durable outcome.
    /// The original publication issuer must permanently close admission.
    #[error("operation journal staging durable outcome is uncertain: {reason}")]
    StagingOutcomeUncertain {
        /// Diagnostic for the unconfirmed commit, rollback or readback.
        reason: String,
    },
    /// A capability-bound incomplete chunk checkpoint failed closed.
    #[error("operation journal chunk staging error: {reason}")]
    ChunkStaging {
        /// Bounded checkpoint diagnostic.
        reason: String,
    },
    /// The requested operation id is not present in the journal.
    #[error("operation journal operation is not present: {operation_id}")]
    OperationNotFound {
        /// Operation identity that was not found.
        operation_id: OperationId,
    },
    /// The journal row changed after the caller observed its sequence.
    #[error(
        "operation journal operation {operation_id} has sequence {actual}, expected {expected}"
    )]
    StaleSequence {
        /// Operation identity whose sequence changed.
        operation_id: OperationId,
        /// Sequence supplied by the caller.
        expected: i64,
        /// Current durable sequence.
        actual: i64,
    },
    /// A different cancellation request is already durable for this operation.
    #[error(
        "operation journal operation {operation_id} already has a different cancellation request"
    )]
    CancellationConflict {
        /// Operation identity with the conflicting request.
        operation_id: OperationId,
    },
    /// The requested cancellation observation is invalid at the store boundary.
    #[error("operation journal cancellation request is invalid: {reason}")]
    InvalidCancellationRequest {
        /// Bounded validation diagnostic.
        reason: String,
    },
    /// This schema cannot be transitioned by the pre-publication cancellation API.
    #[error(
        "operation journal cancellation requires a V3 or V5-V16 universal intent, found schema v{schema_version}"
    )]
    UnsupportedCancellationSchema {
        /// Schema version of the retained row.
        schema_version: u32,
    },
    /// Cancellation is only a user operation in this first transition boundary.
    #[error("operation journal cancellation is unsupported for actor {actor:?}")]
    UnsupportedCancellationActor {
        /// Actor carried by the durable intent.
        actor: OperationActor,
    },
    /// The retained row is not an eligible V3 initial intent.
    #[error("operation journal cancellation state is not eligible: {reason}")]
    UnsupportedCancellationState {
        /// Bounded state diagnostic.
        reason: String,
    },
    /// The requested retry observation is invalid at the store boundary.
    #[error("operation journal retry observation is invalid: {reason}")]
    InvalidRetryObservation {
        /// Bounded validation diagnostic.
        reason: String,
    },
    /// The expected full-progress row sequence was invalid or did not match the retained row.
    #[error(
        "operation journal full-progress sequence conflict for {operation_id}: expected {expected:?}, actual {actual:?}"
    )]
    FullProgressSequenceConflict {
        /// Operation identity carrying the full-progress observation.
        operation_id: OperationId,
        /// Sequence supplied by the caller, when one was supplied.
        expected: Option<i64>,
        /// Sequence currently retained by the full-progress table, when present.
        actual: Option<i64>,
    },
    /// The full-progress observation could not be validated or encoded at the store boundary.
    #[error("operation journal full-progress observation is invalid: {reason}")]
    InvalidFullProgressObservation {
        /// Bounded validation or encoding diagnostic.
        reason: String,
    },
    /// A different retry observation is already durable for this operation.
    #[error("operation journal operation {operation_id} already has a different retry observation")]
    RetryObservationConflict {
        /// Operation identity with the conflicting observation.
        operation_id: OperationId,
    },
    /// This schema cannot be transitioned by the retry API.
    #[error(
        "operation journal retry observation requires a V5-V16 coordinator record, found schema v{schema_version}"
    )]
    UnsupportedRetrySchema {
        /// Schema version of the retained row.
        schema_version: u32,
    },
    /// The retained row is not eligible for a V5-V16 retry observation.
    #[error("operation journal retry state is not eligible: {reason}")]
    UnsupportedRetryState {
        /// Bounded state diagnostic.
        reason: String,
    },
}

/// Acknowledgement returned after one intent has committed and passed post-commit validation.
///
/// The receipt proves persistence in this profile-owned journal only. It does not prove
/// filesystem admission, source/global database admission, or publication authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JournalInsertReceipt {
    operation_id: OperationId,
    sequence: i64,
    encoded_bytes: usize,
}

impl JournalInsertReceipt {
    /// Return the operation id persisted by the insert.
    pub fn operation_id(self) -> OperationId {
        self.operation_id
    }

    /// Return the strictly increasing sequence allocated by the store.
    pub fn sequence(self) -> i64 {
        self.sequence
    }

    /// Return the encoded record length persisted by the store.
    pub fn encoded_bytes(self) -> usize {
        self.encoded_bytes
    }
}

/// The durable write boundary reached by one typed insert attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JournalInsertWriteStage {
    /// Validation or transaction start failed before this attempt could mutate a row.
    ///
    /// This stage does not assert that the operation id is absent: a duplicate or other
    /// pre-existing row is reported by a transaction-stage failure after it has been observed.
    NoMutationAttempted,
    /// The transaction was rolled back before any commit was attempted.
    RolledBackBeforeCommit,
    /// The rollback itself could not be confirmed, so the transaction outcome is uncertain.
    RollbackNotConfirmed,
    /// Commit was attempted but its durable outcome cannot be established.
    CommitOutcomeUnknown,
    /// Commit reported success, but namespace validation or exact readback failed afterward.
    CommittedButNotAcknowledged,
}

/// Failure information for one typed journal insert attempt.
#[derive(Debug, Error)]
#[error("operation journal insert for {operation_id} failed at {stage:?}: {source}")]
pub struct JournalInsertFailure {
    operation_id: OperationId,
    stage: JournalInsertWriteStage,
    source: OperationJournalStoreError,
    attempted_sequence: Option<i64>,
    encoded_bytes: usize,
    rollback_error: Option<Box<OperationJournalStoreError>>,
}

impl JournalInsertFailure {
    /// Return the operation identity supplied to the insert attempt.
    pub const fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    /// Return the durable write boundary reached before failure.
    pub const fn stage(&self) -> JournalInsertWriteStage {
        self.stage
    }

    /// Return the original store error from the failed operation.
    pub fn error(&self) -> &OperationJournalStoreError {
        &self.source
    }

    /// Return the sequence selected before a post-insert failure, when known.
    pub const fn attempted_sequence(&self) -> Option<i64> {
        self.attempted_sequence
    }

    /// Return the encoded record size considered by this attempt.
    pub const fn encoded_bytes(&self) -> usize {
        self.encoded_bytes
    }

    /// Return a rollback error when rollback was attempted but not confirmed.
    pub fn rollback_error(&self) -> Option<&OperationJournalStoreError> {
        self.rollback_error.as_deref()
    }

    /// Consume the typed failure and return its original store error.
    pub fn into_error(self) -> OperationJournalStoreError {
        self.source
    }
}

/// Outcome of an opt-in typed journal insert attempt.
#[derive(Debug)]
pub enum JournalInsertOutcome {
    /// The row committed, passed full post-commit namespace validation, and matched exact
    /// readback.
    Committed(JournalInsertReceipt),
    /// The attempt failed; no tentative receipt is exposed.
    Failed(JournalInsertFailure),
}

impl JournalInsertOutcome {
    /// Return the receipt only when the insert was fully acknowledged.
    pub const fn receipt(&self) -> Option<JournalInsertReceipt> {
        match self {
            Self::Committed(receipt) => Some(*receipt),
            Self::Failed(_) => None,
        }
    }

    /// Borrow failure details, when the attempt did not produce an acknowledgement.
    pub fn failure(&self) -> Option<&JournalInsertFailure> {
        match self {
            Self::Committed(_) => None,
            Self::Failed(failure) => Some(failure),
        }
    }

    /// Convert this outcome to a result while preserving typed failure details.
    pub fn into_result(self) -> Result<JournalInsertReceipt, JournalInsertFailure> {
        match self {
            Self::Committed(receipt) => Ok(receipt),
            Self::Failed(failure) => Err(failure),
        }
    }
}

/// The claim-aware result keeps the owner-thread lease alive after any persistence outcome whose
/// durable state is not fully known.  A confirmed rollback consumes the lease before returning;
/// durable and audit-pending dispositions remain retained after the handle is dropped.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct InitialClaimSettlement {
    outcome: JournalInsertOutcome,
    lease: Option<ProvisionalCapacityLease>,
}

#[allow(dead_code)]
impl InitialClaimSettlement {
    pub(crate) fn outcome(&self) -> &JournalInsertOutcome {
        &self.outcome
    }

    pub(crate) fn lease(&self) -> Option<&ProvisionalCapacityLease> {
        self.lease.as_ref()
    }

    pub(crate) fn into_parts(self) -> (JournalInsertOutcome, Option<ProvisionalCapacityLease>) {
        (self.outcome, self.lease)
    }
}

/// Validation or storage errors before an initial claim settlement can be acknowledged.
#[allow(dead_code)]
#[derive(Debug, Error)]
pub(crate) enum InitialClaimSettlementError {
    #[error("initial capacity plan is invalid: {0}")]
    CapacityPlan(#[from] super::full_capacity::FullCapacityValidationError),
    #[error("initial claim inventory is invalid: {0}")]
    Claims(#[from] AdmissionClaimValidationError),
    #[error("initial capacity admission failed: {0}")]
    Capacity(#[from] CapacityProviderError),
    #[error("initial claim storage validation failed: {0}")]
    ClaimStorage(String),
    #[error("initial claim persistence outcome retained while ledger settlement failed: {source}")]
    OutcomeRetained {
        outcome: JournalInsertOutcome,
        lease: ProvisionalCapacityLease,
        source: CapacityProviderError,
    },
    #[error(transparent)]
    Store(#[from] OperationJournalStoreError),
}

#[allow(dead_code)]
impl InitialClaimSettlementError {
    pub(crate) fn into_retained_parts(
        self,
    ) -> Option<(
        JournalInsertOutcome,
        ProvisionalCapacityLease,
        CapacityProviderError,
    )> {
        match self {
            Self::OutcomeRetained {
                outcome,
                lease,
                source,
            } => Some((outcome, lease, source)),
            _ => None,
        }
    }
}

/// Explicit owner-thread request for logically releasing exactly one durable ordinary claim.
/// The request carries only identities; the claim's volume, class, and bytes are always read
/// from the durable ordinary-claim row before a receipt is created.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OrdinaryClaimReleaseRequest {
    operation_id: OperationId,
    claim_id: AdmissionClaimId,
    release_id: Uuid,
}

#[allow(dead_code)]
impl OrdinaryClaimReleaseRequest {
    pub(crate) fn new(
        operation_id: OperationId,
        claim_id: AdmissionClaimId,
        release_id: Uuid,
    ) -> Result<Self, OrdinaryClaimReleaseError> {
        if operation_id.as_uuid().is_nil() || claim_id.validate().is_err() || release_id.is_nil() {
            return Err(OrdinaryClaimReleaseError::InvalidRequest);
        }
        Ok(Self {
            operation_id,
            claim_id,
            release_id,
        })
    }

    pub(crate) fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    pub(crate) fn claim_id(&self) -> AdmissionClaimId {
        self.claim_id
    }

    pub(crate) fn release_id(&self) -> Uuid {
        self.release_id
    }
}

/// Private receipt returned only after the release relation has committed and read back exactly.
#[allow(dead_code)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OrdinaryClaimReleaseReceipt {
    operation_id: OperationId,
    claim_id: AdmissionClaimId,
    release_id: Uuid,
    volume: VolumeIdentity,
    allocation_class: FullCapacityAllocationClass,
    allocated_bytes: u64,
}

/// One private, consistent durable image used to rebuild the owner-thread provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OrdinaryClaimRebuildSnapshot {
    claims: Vec<RebuildClaim>,
    readiness: CapacityRebuildReadiness,
    /// Structured evidence for the clean-handoff boundary.  This is kept separate from the
    /// human diagnostics used by ordinary rebuild readiness, whose synthetic empty-image message
    /// must not be mistaken for a durable inconsistency.
    durable_zero: bool,
}

/// A claim image can only be constructed by the strict store reader below. Keeping this type in
/// the store module prevents provider callers from manufacturing release history or capacity
/// charges from an observation or arbitrary command payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RebuildClaim {
    operation_id: OperationId,
    claim_id: AdmissionClaimId,
    volume: VolumeIdentity,
    allocation_class: FullCapacityAllocationClass,
    allocated_bytes: u64,
    release_id: Option<Uuid>,
    audit_pending: bool,
}

impl RebuildClaim {
    pub(crate) fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    pub(crate) fn claim_id(&self) -> AdmissionClaimId {
        self.claim_id
    }

    pub(crate) fn volume(&self) -> &VolumeIdentity {
        &self.volume
    }

    pub(crate) fn allocation_class(&self) -> FullCapacityAllocationClass {
        self.allocation_class
    }

    pub(crate) fn allocated_bytes(&self) -> u64 {
        self.allocated_bytes
    }

    pub(crate) fn release_id(&self) -> Option<Uuid> {
        self.release_id
    }

    pub(crate) fn audit_pending(&self) -> bool {
        self.audit_pending
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        operation_id: OperationId,
        claim_id: AdmissionClaimId,
        volume: VolumeIdentity,
        allocation_class: FullCapacityAllocationClass,
        allocated_bytes: u64,
        release_id: Option<Uuid>,
        audit_pending: bool,
    ) -> Self {
        Self {
            operation_id,
            claim_id,
            volume,
            allocation_class,
            allocated_bytes,
            release_id,
            audit_pending,
        }
    }
}

impl OrdinaryClaimRebuildSnapshot {
    pub(crate) fn claims(&self) -> &[RebuildClaim] {
        &self.claims
    }

    pub(crate) fn readiness(&self) -> &CapacityRebuildReadiness {
        &self.readiness
    }

    pub(crate) fn proves_durable_zero(&self) -> bool {
        self.durable_zero
    }

    #[cfg(test)]
    pub(crate) fn for_test(claims: Vec<RebuildClaim>, readiness: CapacityRebuildReadiness) -> Self {
        let durable_zero = claims
            .iter()
            .all(|claim| claim.release_id().is_some() && !claim.audit_pending());
        Self {
            claims,
            readiness,
            durable_zero,
        }
    }
}

#[allow(dead_code)]
impl OrdinaryClaimReleaseReceipt {
    fn from_claim(claim: &ordinary_claims::OrdinaryClaimRecord, release_id: Uuid) -> Self {
        Self {
            operation_id: claim.operation_id(),
            claim_id: claim.claim_id(),
            release_id,
            volume: claim.volume().clone(),
            allocation_class: claim.allocation_class(),
            allocated_bytes: claim.allocated_bytes(),
        }
    }

    pub(crate) fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    pub(crate) fn claim_id(&self) -> AdmissionClaimId {
        self.claim_id
    }

    pub(crate) fn release_id(&self) -> Uuid {
        self.release_id
    }

    pub(crate) fn volume(&self) -> &VolumeIdentity {
        &self.volume
    }

    pub(crate) fn allocation_class(&self) -> FullCapacityAllocationClass {
        self.allocation_class
    }

    pub(crate) fn allocated_bytes(&self) -> u64 {
        self.allocated_bytes
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        operation_id: OperationId,
        claim_id: AdmissionClaimId,
        release_id: Uuid,
        volume: VolumeIdentity,
        allocation_class: FullCapacityAllocationClass,
        allocated_bytes: u64,
    ) -> Self {
        Self {
            operation_id,
            claim_id,
            release_id,
            volume,
            allocation_class,
            allocated_bytes,
        }
    }
}

#[derive(Debug, Error)]
#[allow(dead_code)]
pub(crate) enum OrdinaryClaimReleaseError {
    #[error("ordinary claim release request contains a nil or invalid identity")]
    InvalidRequest,
    #[error("ordinary claim {claim_id} was not found")]
    ClaimNotFound { claim_id: AdmissionClaimId },
    #[error("ordinary claim {claim_id} belongs to a different operation")]
    OperationMismatch { claim_id: AdmissionClaimId },
    #[error("ordinary claim {claim_id} is a chunk claim and requires its owner transition")]
    ChunkClaimRequiresOwner { claim_id: AdmissionClaimId },
    #[error("ordinary claim {claim_id} already has a conflicting release receipt")]
    ReleaseConflict { claim_id: AdmissionClaimId },
    #[error("ordinary claim release persistence failed: {source}")]
    Persistence {
        #[source]
        source: OperationJournalStoreError,
    },
    #[error("ordinary claim release persistence outcome is uncertain: {source}")]
    PersistenceUncertain {
        #[source]
        source: OperationJournalStoreError,
    },
    #[error("ordinary claim release receipt committed but provider retained its charge: {source}")]
    ProviderRetained {
        receipt: OrdinaryClaimReleaseReceipt,
        #[source]
        source: CapacityProviderError,
    },
}

/// Acknowledgement returned after a cancellation request has committed and passed readback.
///
/// The receipt proves only that the non-terminal request is durable in this profile-owned
/// journal. It grants no cleanup, capacity-release, filesystem, or recovery execution authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JournalCancelRequestReceipt {
    operation_id: OperationId,
    sequence: i64,
    encoded_bytes: usize,
}

impl JournalCancelRequestReceipt {
    /// Return the operation id carrying the cancellation request.
    pub fn operation_id(self) -> OperationId {
        self.operation_id
    }

    /// Return the new or replayed journal sequence.
    pub fn sequence(self) -> i64 {
        self.sequence
    }

    /// Return the encoded V4, V5, or V6 record length.
    pub fn encoded_bytes(self) -> usize {
        self.encoded_bytes
    }
}

/// Acknowledgement returned after a typed retry observation has committed and passed readback.
///
/// The receipt proves only that scheduling metadata is durable in this profile-owned journal. It
/// grants no live retry lease, capacity reservation, filesystem capability, or execution
/// authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JournalRetryReceipt {
    operation_id: OperationId,
    sequence: i64,
    encoded_bytes: usize,
}

impl JournalRetryReceipt {
    /// Return the operation carrying the retry observation.
    pub fn operation_id(self) -> OperationId {
        self.operation_id
    }

    /// Return the new or replayed journal sequence.
    pub fn sequence(self) -> i64 {
        self.sequence
    }

    /// Return the encoded V5 or V6 record length.
    pub fn encoded_bytes(self) -> usize {
        self.encoded_bytes
    }
}

struct JournalRow {
    sequence: i64,
    encoded: Vec<u8>,
    record: StoredOperationRecord,
}

enum CancelTransition {
    Replay(JournalCancelRequestReceipt),
    Updated {
        receipt: JournalCancelRequestReceipt,
        record: StoredOperationRecord,
    },
}

enum RetryTransition {
    Replay(JournalRetryReceipt),
    Updated {
        receipt: JournalRetryReceipt,
        record: StoredOperationRecord,
    },
}

#[allow(dead_code)]
enum OrdinaryClaimReleaseTransition {
    Replay(OrdinaryClaimReleaseReceipt),
    Inserted(OrdinaryClaimReleaseReceipt),
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InsertTestBoundary {
    BeforeBegin,
    BeforePrecommitValidation,
    Rollback,
    Commit,
    AfterCommit,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReleaseTestBoundary {
    BeforeBegin,
    BeforePrecommitValidation,
    Rollback,
    Commit,
    AfterCommit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MigrationTestBoundary {
    BeforeMigration,
    AfterFullProgressTable,
    BeforeCommit,
}

struct OpenHooks<'a> {
    #[cfg(test)]
    before_sqlite_open: Option<&'a dyn Fn(&Path)>,
    migration_failure: Option<
        &'a dyn Fn(MigrationTestBoundary, &Connection) -> Option<OperationJournalStoreError>,
    >,
}

#[cfg(test)]
pub(crate) enum InsertTestDirective {
    Continue,
    Fail(OperationJournalStoreError),
    FailAndLeaveForRollback(OperationJournalStoreError),
    ExecuteThenFail(OperationJournalStoreError),
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) enum ReleaseTestDirective {
    Continue,
    Fail(OperationJournalStoreError),
    FailAndLeaveForRollback(OperationJournalStoreError),
    ExecuteThenFail(OperationJournalStoreError),
}

/// One profile-owned operation-journal SQLite connection.
///
/// The retained root binding and cloned profile owner are kept until after the SQLite connection
/// is dropped. The store has no raw-connection escape hatch and exposes only bounded point reads.
pub struct OperationJournalStore {
    connection: Connection,
    recording_prestart_scope: std::cell::Cell<bool>,
    recording_prestart_resource_refused: std::cell::Cell<bool>,
    database_path: PathBuf,
    database_identity: String,
    database_root: BoundDatabaseRoot,
    profile_guard: WritableProfileGuard,
    #[cfg(test)]
    recording_readback_corruption: std::sync::Mutex<Option<uuid::Uuid>>,
    #[cfg(test)]
    current_reserve_test_hook: std::sync::Mutex<Option<CurrentReserveHook>>,
    #[cfg(all(test, unix))]
    current_reopen_fixture: std::cell::Cell<bool>,
    #[cfg(test)]
    insert_test_hook:
        std::sync::Mutex<Option<Box<dyn FnMut(InsertTestBoundary) -> InsertTestDirective + Send>>>,
    #[cfg(test)]
    release_test_hook: std::sync::Mutex<
        Option<Box<dyn FnMut(ReleaseTestBoundary) -> ReleaseTestDirective + Send>>,
    >,
}

/// Private live capability context for recovery-reserve ledger transitions.
///
/// The context retains the profile-owned journal, the per-user volume lock, and the owner-bound
/// physical reserve together. It is intentionally crate-private: the ordinary admission gate
/// cannot construct or use it until the complete claim and physical-spend boundary is wired.
#[allow(dead_code)]
pub(crate) struct RecoveryReserveContext<'a> {
    store: &'a OperationJournalStore,
    volume_owner: VolumeOwnershipCapability,
    reserve: BoundReserve<'a>,
    journal_identity: String,
    profile_identity: String,
}

#[derive(Debug, Error)]
pub(crate) enum ProfileJournalReserveBootstrapError {
    #[error("journal root descriptor could not be duplicated: {0}")]
    RootDescriptor(String),
    #[error(transparent)]
    VolumeOwnership(#[from] VolumeOwnershipError),
    #[error("protected reserve namespace bootstrap failed: {0}")]
    Namespace(String),
    #[error("live reserve validation failed during bootstrap: {0}")]
    NamespaceValidation(String),
    #[error(transparent)]
    Recovery(#[from] RecoveryReserveContextError),
    #[error(transparent)]
    Binding(#[from] recovery_reserve_binding::ReserveBindingError),
    #[error(transparent)]
    Store(#[from] OperationJournalStoreError),
}

#[allow(dead_code)]
#[derive(Debug, Error)]
pub(crate) enum RecoveryReserveContextError {
    #[error(transparent)]
    Store(#[from] OperationJournalStoreError),
    #[error(transparent)]
    VolumeOwnership(#[from] VolumeOwnershipError),
    #[error("recovery-reserve context profile identity does not match the journal owner")]
    ProfileIdentityMismatch,
    #[error("recovery-reserve context volume identity does not match the live reserve")]
    VolumeIdentityMismatch,
    #[error("recovery-reserve context journal identity changed while retained")]
    JournalIdentityChanged,
    #[error("recovery-reserve physical proof is unavailable: {0}")]
    Reserve(String),
    #[error(transparent)]
    Ledger(#[from] recovery_reserve::LedgerError),
    #[error(transparent)]
    Binding(#[from] recovery_reserve_binding::ReserveBindingError),
}

// Closed original bootstrap capabilities. Existing current-lease gates remain unchanged.
#[derive(Default)]
pub(super) struct LegacyReserveFailureSink {
    namespace:
        std::cell::RefCell<Option<super::capacity_reserve_namespace::LegacyReserveFailureBacking>>,
    reserve: std::cell::RefCell<Option<OwnedReserveAllocation>>,
}
#[cfg(all(test, unix))]
impl LegacyReserveFailureSink {
    pub(super) fn opened_reserve_for_test(
        &self,
    ) -> std::cell::Ref<'_, Option<OwnedReserveAllocation>> {
        self.reserve.borrow()
    }
    pub(super) fn retain_opened_for_test(&self, reserve: OwnedReserveAllocation) {
        assert!(self.reserve.borrow().is_none());
        *self.reserve.borrow_mut() = Some(reserve);
    }
}
pub(super) struct LegacyReserveBindingFailure {
    pub(super) original: SourceReserveBindingFailure,
    pub(super) namespace: Option<super::capacity_reserve_namespace::LegacyReserveFailureBacking>,
    pub(super) reopened_reserve: Option<OwnedReserveAllocation>,
}
enum ReserveBootstrapProof<'a, 'b> {
    #[cfg(all(test, target_os = "macos"))]
    Current(&'a SourceAuthorityUseProof),
    ScopedCurrent {
        source: &'a crate::sample_sources::db::CurrentReserveUseProof<'b>,
        failure: &'a LegacyReserveFailureSink,
    },
    Legacy {
        source: &'a crate::sample_sources::db::LegacyReserveUseProof<'b>,
        failure: &'a LegacyReserveFailureSink,
    },
}
impl ReserveBootstrapProof<'_, '_> {
    fn validate_for_physical_use(&self) -> Result<(), String> {
        match self {
            #[cfg(all(test, target_os = "macos"))]
            Self::Current(proof) => proof.validate_for_physical_use().map_err(|e| e.to_string()),
            Self::ScopedCurrent { source, .. } => source
                .validate_root_for_bootstrap(source.root())
                .map_err(|e| e.to_string()),
            Self::Legacy { source, .. } => {
                source.validate_for_bootstrap().map_err(|e| e.to_string())
            }
        }
    }
    fn begin_effect(
        &self,
    ) -> Result<Option<crate::sample_sources::db::SourceEffectPermit>, RecoveryReserveContextError>
    {
        match self {
            Self::ScopedCurrent { source, .. } => source
                .begin_bootstrap_effect()
                .map(Some)
                .map_err(|e| RecoveryReserveContextError::Reserve(e.to_string())),
            _ => {
                self.validate_for_physical_use()
                    .map_err(RecoveryReserveContextError::Reserve)?;
                Ok(None)
            }
        }
    }
    fn namespace_error(
        &self,
        error: super::capacity_reserve_namespace::ReserveNamespaceFailure,
    ) -> RecoveryReserveContextError {
        let text = match self {
            Self::Legacy { failure, .. } | Self::ScopedCurrent { failure, .. } => {
                let text = format!(
                    "source reserve failure at {:?}; exact backing retained",
                    error.stage()
                );
                *failure.namespace.borrow_mut() = Some(error.into_legacy_backing());
                text
            }
            #[cfg(all(test, target_os = "macos"))]
            Self::Current(_) => error.to_string(),
        };
        RecoveryReserveContextError::Reserve(text)
    }
    fn failure_sink(&self) -> Option<&LegacyReserveFailureSink> {
        match self {
            Self::Legacy { failure, .. } | Self::ScopedCurrent { failure, .. } => Some(failure),
            #[cfg(all(test, target_os = "macos"))]
            Self::Current(_) => None,
        }
    }
    fn retain_reopened(&self, reserve: OwnedReserveAllocation) {
        if let Some(failure) = self.failure_sink() {
            *failure.reserve.borrow_mut() = Some(reserve);
        }
    }
    fn retain_bound(&self, reserve: super::capacity_reserve_namespace::BoundReserve<'_>) {
        if self.failure_sink().is_none() {
            return;
        }
        match reserve.into_owned() {
            Ok(reserve) => self.retain_reopened(reserve),
            Err(error) => {
                self.namespace_error(error);
            }
        }
    }
    fn create_reserve<'a>(
        &self,
        root: &'a BoundDatabaseRoot,
        profile: &'a WritableProfileGuard,
    ) -> Result<
        super::capacity_reserve_namespace::BoundReserve<'a>,
        super::capacity_reserve_namespace::ReserveNamespaceFailure,
    > {
        match self {
            #[cfg(all(test, target_os = "macos"))]
            Self::Current(_) => {
                super::capacity_reserve_namespace::create_owned_reserve(root, profile)
            }
            Self::ScopedCurrent { source, .. } => {
                super::capacity_reserve_namespace::create_owned_current_reserve(
                    root, profile, source,
                )
            }
            Self::Legacy { source, .. } => {
                super::capacity_reserve_namespace::create_owned_legacy_reserve(
                    root, profile, source,
                )
            }
        }
    }
}

/// Failure while binding one source-volume reserve, retaining every owner-thread resource that
/// may have been acquired before the error. The journal owner converts this into a recovery-only
/// registration state instead of dropping a lock or reserve after an ambiguous write boundary.
pub(crate) struct SourceReserveBindingFailure {
    pub(crate) error: RecoveryReserveContextError,
    pub(crate) reserve: Option<OwnedReserveAllocation>,
    pub(crate) volume_owner: VolumeOwnershipCapability,
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PhysicalMutationFault {
    None,
    #[cfg(test)]
    BeforeTruncate,
    #[cfg(test)]
    AfterTruncateBeforeSync,
    #[cfg(test)]
    Allocate,
    #[cfg(test)]
    Sync,
}

#[cfg(target_os = "macos")]
impl PhysicalMutationFault {
    fn reserve_fault(self) -> super::capacity_reserve::MutationFault {
        match self {
            Self::None => super::capacity_reserve::MutationFault::None,
            #[cfg(test)]
            Self::BeforeTruncate => super::capacity_reserve::MutationFault::BeforeTruncate,
            #[cfg(test)]
            Self::AfterTruncateBeforeSync => {
                super::capacity_reserve::MutationFault::AfterTruncateBeforeSync
            }
            #[cfg(test)]
            Self::Allocate => super::capacity_reserve::MutationFault::Allocate,
            #[cfg(test)]
            Self::Sync => super::capacity_reserve::MutationFault::Sync,
        }
    }
}

#[allow(dead_code)]
impl<'a> RecoveryReserveContext<'a> {
    fn into_owned_parts(
        self,
    ) -> Result<(VolumeOwnershipCapability, OwnedReserveAllocation), RecoveryReserveContextError>
    {
        let RecoveryReserveContext {
            volume_owner,
            reserve,
            ..
        } = self;
        let reserve = reserve
            .into_owned()
            .map_err(|error| RecoveryReserveContextError::Reserve(error.to_string()))?;
        Ok((volume_owner, reserve))
    }

    fn into_owned_reserve(self) -> Result<OwnedReserveAllocation, RecoveryReserveContextError> {
        self.into_owned_parts().map(|(_, reserve)| reserve)
    }

    fn validate_binding(&self) -> Result<(), RecoveryReserveContextError> {
        let live = recovery_reserve_binding::ReserveBinding::from_live(
            self.volume_owner.volume().clone(),
            self.profile_identity.as_str(),
            self.reserve.observation().root_identity(),
            self.reserve.observation().file_identity(),
            self.reserve.observation().allocation_unit(),
            self.volume_owner.owner_generation(),
        )?;
        let stored =
            recovery_reserve_binding::read(&self.store.connection, self.volume_owner.volume())?
                .ok_or_else(|| {
                    recovery_reserve_binding::ReserveBindingError::Missing(
                        self.volume_owner.volume().clone(),
                    )
                })?;
        if stored != live {
            return Err(RecoveryReserveContextError::Binding(
                recovery_reserve_binding::ReserveBindingError::IdentityMismatch(
                    "stored reserve binding differs from the live owner-bound observation".into(),
                ),
            ));
        }
        Ok(())
    }

    fn validate_live(&self, validate_reserve: bool) -> Result<(), RecoveryReserveContextError> {
        self.store.validate_namespace()?;
        if self.store.database_identity != self.journal_identity {
            return Err(RecoveryReserveContextError::JournalIdentityChanged);
        }
        let profile_identity = self.store.profile_guard.profile_root_identity();
        if profile_identity != self.profile_identity
            || self.volume_owner.owner_profile_identity() != profile_identity
            || self.reserve.profile_identity() != profile_identity
        {
            return Err(RecoveryReserveContextError::ProfileIdentityMismatch);
        }
        if self.volume_owner.volume() != self.reserve.observation().volume() {
            return Err(RecoveryReserveContextError::VolumeIdentityMismatch);
        }
        self.validate_binding()?;
        self.volume_owner.revalidate()?;
        if validate_reserve {
            self.reserve
                .revalidate()
                .map_err(|error| RecoveryReserveContextError::Reserve(error.to_string()))?;
        }
        Ok(())
    }

    pub(crate) fn begin_charge(
        &self,
        request: &recovery_reserve::ChargeRequest,
    ) -> Result<recovery_reserve::ChargeSnapshot, RecoveryReserveContextError> {
        self.validate_live(true)?;
        if request.volume != *self.volume_owner.volume() {
            return Err(RecoveryReserveContextError::VolumeIdentityMismatch);
        }
        let fresh_facts = self
            .reserve
            .fresh_capacity_facts()
            .map_err(|error| RecoveryReserveContextError::Reserve(error.to_string()))?;
        let mut factual_request = request.clone();
        factual_request.precharge_capacity_bytes = fresh_facts.free_bytes;
        Ok(recovery_reserve::begin_charge(
            &self.store.connection,
            &factual_request,
        )?)
    }

    pub(crate) fn spend(
        &self,
        charge_id: uuid::Uuid,
        transition_id: uuid::Uuid,
        amount_bytes: u64,
    ) -> Result<recovery_reserve::ChargeSnapshot, RecoveryReserveContextError> {
        self.validate_live(true)?;
        Ok(recovery_reserve::spend(
            &self.store.connection,
            self.volume_owner.volume(),
            charge_id,
            transition_id,
            amount_bytes,
        )?)
    }

    pub(crate) fn record_release_observation(
        &self,
        charge_id: uuid::Uuid,
        transition_id: uuid::Uuid,
        amount_bytes: u64,
    ) -> Result<recovery_reserve::ChargeSnapshot, RecoveryReserveContextError> {
        self.validate_live(true)?;
        Ok(recovery_reserve::record_release_observation(
            &self.store.connection,
            self.volume_owner.volume(),
            charge_id,
            transition_id,
            amount_bytes,
        )?)
    }

    pub(crate) fn reconstitute(
        &self,
        charge_id: uuid::Uuid,
        transition_id: uuid::Uuid,
    ) -> Result<recovery_reserve::ChargeSnapshot, RecoveryReserveContextError> {
        self.validate_live(false)?;
        let proof: PhysicalReserveProof = self
            .reserve
            .revalidate_and_prove()
            .map_err(|error| RecoveryReserveContextError::Reserve(error.to_string()))?;
        Ok(recovery_reserve::reconstitute(
            &self.store.connection,
            self.volume_owner.volume(),
            charge_id,
            transition_id,
            &proof,
        )?)
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn spend_physical(
        &mut self,
        charge_id: uuid::Uuid,
        transition_id: uuid::Uuid,
        requested_bytes: u64,
    ) -> Result<recovery_reserve::ChargeSnapshot, RecoveryReserveContextError> {
        self.spend_physical_impl(
            charge_id,
            transition_id,
            requested_bytes,
            PhysicalMutationFault::None,
        )
    }

    #[cfg(all(test, target_os = "macos"))]
    pub(super) fn spend_physical_with_test_fault(
        &mut self,
        charge_id: uuid::Uuid,
        transition_id: uuid::Uuid,
        requested_bytes: u64,
        fault: super::capacity_reserve::TestReserveMutationFault,
    ) -> Result<recovery_reserve::ChargeSnapshot, RecoveryReserveContextError> {
        let fault = match fault {
            super::capacity_reserve::TestReserveMutationFault::BeforeTruncate => {
                PhysicalMutationFault::BeforeTruncate
            }
            super::capacity_reserve::TestReserveMutationFault::AfterTruncateBeforeSync => {
                PhysicalMutationFault::AfterTruncateBeforeSync
            }
            super::capacity_reserve::TestReserveMutationFault::Allocate => {
                PhysicalMutationFault::Allocate
            }
            super::capacity_reserve::TestReserveMutationFault::Sync => PhysicalMutationFault::Sync,
        };
        self.spend_physical_impl(charge_id, transition_id, requested_bytes, fault)
    }

    #[cfg(target_os = "macos")]
    fn spend_physical_impl(
        &mut self,
        charge_id: uuid::Uuid,
        transition_id: uuid::Uuid,
        requested_bytes: u64,
        fault: PhysicalMutationFault,
    ) -> Result<recovery_reserve::ChargeSnapshot, RecoveryReserveContextError> {
        self.validate_live(false)?;
        self.reserve
            .revalidate_partial()
            .map_err(|error| RecoveryReserveContextError::Reserve(error.to_string()))?;
        let facts = self
            .reserve
            .fresh_capacity_facts()
            .map_err(|error| RecoveryReserveContextError::Reserve(error.to_string()))?;
        let before = recovery_reserve::charge_snapshot_for_volume(
            &self.store.connection,
            self.volume_owner.volume(),
            charge_id,
        )?;
        let rounded =
            crate::durable_capacity::round_up_allocation(requested_bytes, facts.allocation_unit)
                .map_err(|error| RecoveryReserveContextError::Reserve(error.to_string()))?;
        if rounded == 0 {
            return Err(RecoveryReserveContextError::Reserve(
                "physical spend must round to a positive allocation".into(),
            ));
        }
        // Durable accounting is committed before the descriptor extent changes. A failed
        // physical release therefore leaves a recoverable Spent head and never gets rolled back.
        // The ledger's remaining-budget check is stricter than the remaining-plus-margin bound;
        // its charge invariant keeps the margin below the protected floor. It must run before any
        // retry-specific physical handling so a matching transition remains idempotent after
        // later spends have reduced the current remaining budget.
        let outcome = recovery_reserve::spend_with_outcome(
            &self.store.connection,
            self.volume_owner.volume(),
            charge_id,
            transition_id,
            rounded,
        )?;
        if outcome.head.state == recovery_reserve::RecoveryReserveState::Reconstituted {
            return Ok(outcome.snapshot);
        }
        recovery_reserve::require_active_charge_for_volume(
            &self.store.connection,
            self.volume_owner.volume(),
            charge_id,
        )?;
        let spent_after = outcome
            .head
            .budget_bytes
            .checked_sub(outcome.head.remaining_bytes)
            .ok_or_else(|| {
                RecoveryReserveContextError::Reserve(
                    "durable spend head exceeds its charge budget".into(),
                )
            })?;
        let target_length = crate::durable_capacity::PROTECTED_FREE_SPACE_FLOOR
            .checked_sub(spent_after)
            .ok_or_else(|| {
                RecoveryReserveContextError::Reserve(
                    "durable spend would consume the protected floor".into(),
                )
            })?;
        if target_length < outcome.head.margin_bytes {
            return Err(RecoveryReserveContextError::Reserve(
                "durable spend would consume the control margin".into(),
            ));
        }

        if outcome.applied {
            let spent_before = before
                .budget_bytes
                .checked_sub(before.remaining_bytes)
                .ok_or_else(|| {
                    RecoveryReserveContextError::Reserve(
                        "durable spend head has invalid prior budget".into(),
                    )
                })?;
            let expected_current_length = crate::durable_capacity::PROTECTED_FREE_SPACE_FLOOR
                .checked_sub(spent_before)
                .ok_or_else(|| {
                    RecoveryReserveContextError::Reserve(
                        "prior spend exceeds the protected floor".into(),
                    )
                })?;
            self.reserve
                .shrink_to_with_mutation_fault(
                    expected_current_length,
                    target_length,
                    fault.reserve_fault(),
                )
                .map_err(|error| RecoveryReserveContextError::Reserve(error.to_string()))?;
        } else {
            self.reserve
                .ensure_extent_at_most_with_mutation_fault(target_length, fault.reserve_fault())
                .map_err(|error| RecoveryReserveContextError::Reserve(error.to_string()))?;
        }
        Ok(outcome.snapshot)
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn reconstitute_physical(
        &mut self,
        charge_id: uuid::Uuid,
        transition_id: uuid::Uuid,
    ) -> Result<recovery_reserve::ChargeSnapshot, RecoveryReserveContextError> {
        self.reconstitute_physical_impl(charge_id, transition_id, PhysicalMutationFault::None)
    }

    #[cfg(all(test, target_os = "macos"))]
    pub(super) fn reconstitute_physical_with_test_fault(
        &mut self,
        charge_id: uuid::Uuid,
        transition_id: uuid::Uuid,
        fault: super::capacity_reserve::TestReserveMutationFault,
    ) -> Result<recovery_reserve::ChargeSnapshot, RecoveryReserveContextError> {
        let fault = match fault {
            super::capacity_reserve::TestReserveMutationFault::BeforeTruncate => {
                PhysicalMutationFault::BeforeTruncate
            }
            super::capacity_reserve::TestReserveMutationFault::AfterTruncateBeforeSync => {
                PhysicalMutationFault::AfterTruncateBeforeSync
            }
            super::capacity_reserve::TestReserveMutationFault::Allocate => {
                PhysicalMutationFault::Allocate
            }
            super::capacity_reserve::TestReserveMutationFault::Sync => PhysicalMutationFault::Sync,
        };
        self.reconstitute_physical_impl(charge_id, transition_id, fault)
    }

    #[cfg(target_os = "macos")]
    fn reconstitute_physical_impl(
        &mut self,
        charge_id: uuid::Uuid,
        transition_id: uuid::Uuid,
        fault: PhysicalMutationFault,
    ) -> Result<recovery_reserve::ChargeSnapshot, RecoveryReserveContextError> {
        self.validate_live(false)?;
        let preflight = recovery_reserve::reconstitute_preflight(
            &self.store.connection,
            self.volume_owner.volume(),
            charge_id,
            transition_id,
        )?;
        if let recovery_reserve::ReconstitutePreflight::Historical(snapshot) = preflight {
            return Ok(snapshot);
        }
        let _ = self
            .reserve
            .revalidate_partial()
            .map_err(|error| RecoveryReserveContextError::Reserve(error.to_string()))?;
        self.reserve
            .reconstitute_to_floor_with_mutation_fault(fault.reserve_fault())
            .map_err(|error| RecoveryReserveContextError::Reserve(error.to_string()))?;
        let proof = self
            .reserve
            .revalidate_and_prove()
            .map_err(|error| RecoveryReserveContextError::Reserve(error.to_string()))?;
        Ok(recovery_reserve::reconstitute(
            &self.store.connection,
            self.volume_owner.volume(),
            charge_id,
            transition_id,
            &proof,
        )?)
    }
}

impl std::fmt::Debug for OperationJournalStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OperationJournalStore")
            .field("database_path", &self.database_path)
            .finish_non_exhaustive()
    }
}

impl OperationJournalStore {
    #[cfg(test)]
    pub(crate) fn legacy_binding_prefix_for_test(&self, prefix: u64) {
        // Exact passive journal-prefix logic test only: force page growth using bounded valid
        // binding rows. These rows are never physical qualification or live volume authority.
        let identity = "i".repeat(256);
        for n in 0..64u64 {
            self.connection.execute(
                "INSERT INTO operation_journal_recovery_reserve_binding VALUES (?1, ?2, ?2, ?2, 4096, 268435456, 1, 'recovery-reserve.bin', 1)",
                rusqlite::params![(prefix * 64 + n + 1000).to_string(), identity],
            ).unwrap();
        }
    }
    /// Open or create the journal under the retained operation_journal profile child.
    ///
    /// Existing files are accepted only when their SQLite header and private schema match this
    /// store. Missing files are created with no-follow create_new; failures during bootstrap
    /// leave the file in place so a later owner can audit it rather than silently replacing it.
    pub fn open(profile_guard: &WritableProfileGuard) -> Result<Self, OperationJournalStoreError> {
        Self::open_inner(
            profile_guard,
            #[cfg(test)]
            None,
        )
    }

    /// Read one bounded, read-only recovery page after an optional journal-observation cursor.
    ///
    /// The page validates the retained namespace before and after its query. Returned records
    /// are untrusted observations and do not authorize filesystem or journal mutations.
    pub fn scan_recovery_page(
        &self,
        cursor: Option<recovery::RecoveryCursor>,
    ) -> Result<recovery::RecoveryPage, recovery::RecoveryScanError> {
        recovery::scan_page(self, cursor)
    }

    /// Read one bounded combined recovery page from the legacy and full-progress families.
    ///
    /// This crate-internal observation path never restores a live progress value or grants any
    /// owner, permit, admission, or filesystem authority.
    pub(crate) fn scan_coordinator_recovery_page(
        &self,
        cursor: Option<recovery::RecoveryCursor>,
    ) -> Result<recovery::CoordinatorRecoveryPage, recovery::RecoveryScanError> {
        recovery::scan_coordinator_page(self, cursor)
    }

    /// Return a bounded read-only startup projection over both journal families.
    ///
    /// A blocked result is conservative: it does not restore a record, grant recovery authority,
    /// or permit writable startup to proceed.
    pub fn recovery_readiness(
        &self,
    ) -> Result<recovery::OperationJournalRecoveryReadiness, recovery::RecoveryScanError> {
        recovery::recovery_readiness(self)
    }

    /// Read a bounded, immutable ordinary-claim observation page.
    ///
    /// Claim rows are sorted by their canonical UUID text and are never interpreted as live
    /// capacity authority. A zero limit is an explicit empty page; larger requests are rejected
    /// before SQLite is queried.
    #[allow(dead_code)]
    pub(crate) fn read_ordinary_claim_page(
        &self,
        cursor: Option<OrdinaryClaimCursor>,
        limit: usize,
    ) -> Result<OrdinaryClaimPage, OrdinaryClaimReadError> {
        self.validate_namespace()
            .map_err(|error| OrdinaryClaimReadError::Namespace(error.to_string()))?;
        let page = ordinary_claims::read_page(&self.connection, cursor, limit)?;
        self.validate_namespace()
            .map_err(|error| OrdinaryClaimReadError::Namespace(error.to_string()))?;
        Ok(page)
    }

    /// Point-read original claim evidence, rejecting a recorded release. This grants no authority.
    pub(crate) fn read_active_ordinary_claim(
        &self,
        claim_id: AdmissionClaimId,
    ) -> Result<Option<ordinary_claims::OrdinaryClaimRecord>, OrdinaryClaimReadError> {
        self.validate_namespace()
            .map_err(|error| OrdinaryClaimReadError::Namespace(error.to_string()))?;
        let claim = ordinary_claims::read_claim(&self.connection, claim_id)?;
        let released = ordinary_claims::read_release(&self.connection, claim_id)?.is_some();
        self.validate_namespace()
            .map_err(|error| OrdinaryClaimReadError::Namespace(error.to_string()))?;
        Ok(if released { None } else { claim })
    }

    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn insert_ordinary_claim_for_test(
        &self,
        claim_id: Uuid,
        operation_id: OperationId,
        volume: &VolumeIdentity,
        allocation_class: &str,
        allocated_bytes: u64,
    ) -> Result<(), OperationJournalStoreError> {
        self.connection.execute(
            &format!(
                "INSERT INTO {table}
                 (claim_id, operation_id, volume_device, allocation_class, allocated_bytes)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                table = ordinary_claims::TABLE_NAME
            ),
            params![
                claim_id.to_string(),
                operation_id.to_string(),
                volume.device.to_string(),
                allocation_class,
                i64::try_from(allocated_bytes).map_err(|_| {
                    OperationJournalStoreError::InconsistentRecord {
                        reason: String::from("test claim bytes exceed SQLite INTEGER"),
                    }
                })?,
            ],
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn insert_active_recovery_charge_for_test(
        &self,
        operation_id: OperationId,
        volume: &VolumeIdentity,
        budget_bytes: u64,
    ) -> Result<(), OperationJournalStoreError> {
        self.validate_namespace()?;
        let charge_id = Uuid::new_v4();
        let transition_id = Uuid::new_v4();
        let budget = i64::try_from(budget_bytes).map_err(|_| {
            OperationJournalStoreError::InconsistentRecord {
                reason: String::from("test recovery budget exceeds SQLite INTEGER"),
            }
        })?;
        self.connection.execute(
            &format!(
                "INSERT INTO {table}
                 (charge_id, volume_device, active_volume_device, operation_id, purpose,
                  budget_bytes, margin_bytes, precharge_capacity_bytes, remaining_bytes,
                  state, transition_sequence)
                 VALUES (?1, ?2, ?2, ?3, 'OperationRecovery', ?4, 0, ?5, ?4,
                         'Charged', 1)",
                table = recovery_reserve::CHARGE_TABLE_NAME
            ),
            params![
                charge_id.to_string(),
                volume.device.to_string(),
                operation_id.to_string(),
                budget,
                budget,
            ],
        )?;
        self.connection.execute(
            &format!(
                "INSERT INTO {table}
                 (charge_id, transition_id, transition_sequence, kind, amount_bytes,
                  remaining_bytes, state)
                 VALUES (?1, ?2, 1, 'Charged', ?3, ?3, 'Charged')",
                table = recovery_reserve::TRANSITION_TABLE_NAME
            ),
            params![charge_id.to_string(), transition_id.to_string(), budget],
        )?;
        Ok(())
    }

    /// Read one consistent private ordinary-claim image for owner-thread restart rebuilding.
    ///
    /// The read transaction uses the existing bounded UUID pages for both claim relations and
    /// point-validates every referenced legacy operation.  It never changes the durable rows or
    /// turns a recovery observation into ordinary admission authority.
    pub(crate) fn rebuild_ordinary_claim_snapshot(
        &self,
    ) -> Result<OrdinaryClaimRebuildSnapshot, OperationJournalStoreError> {
        self.rebuild_ordinary_startup_snapshot_with_funding(None)
            .map(FundedSnapshot::into_unfunded)
    }

    pub(super) fn rebuild_ordinary_startup_snapshot_for_input(
        &self,
        input: &super::owner::InputPermit,
    ) -> Result<FundedSnapshot, OperationJournalStoreError> {
        self.rebuild_ordinary_startup_snapshot_with_funding(Some(input))
    }

    fn rebuild_ordinary_startup_snapshot_with_funding(
        &self,
        input: Option<&super::owner::InputPermit>,
    ) -> Result<FundedSnapshot, OperationJournalStoreError> {
        self.validate_namespace()?;
        self.connection.execute_batch("BEGIN")?;
        // Original transaction/validation order; credit precedes all working backing.
        let mut collection = None;
        let snapshot =
            match self.rebuild_ordinary_claim_snapshot_with_funding(input, &mut collection) {
                Ok(snapshot) => {
                    if let Err(error) = self.validate_chunk_claims_inside_transaction() {
                        let _ = self.connection.execute_batch("ROLLBACK");
                        return Err(error);
                    }
                    snapshot
                }
                Err(error) => {
                    let _ = self.connection.execute_batch("ROLLBACK");
                    return Err(error);
                }
            };
        if let Err(error) = self.validate_namespace_for_precommit() {
            let _ = self.connection.execute_batch("ROLLBACK");
            return Err(error);
        }
        self.connection.execute_batch("ROLLBACK")?;
        self.validate_namespace()?;
        Ok(FundedSnapshot::new(
            snapshot,
            collection.map(admission_rebuild_backing::Collection::into_credit),
        ))
    }

    /// Read the ordinary-claim image and recovery-reserve history in one consistent admission
    /// audit. Ordinary durable charges may coexist with a new request, but any active, malformed,
    /// or otherwise unresolved recovery-reserve row keeps the owner recovery-only. This method
    /// only observes durable state; it never installs a provider image or changes accounting.
    pub(crate) fn rebuild_admission_snapshot(
        &self,
    ) -> Result<OrdinaryClaimRebuildSnapshot, OperationJournalStoreError> {
        self.rebuild_admission_snapshot_with_funding(None)
            .map(FundedSnapshot::into_unfunded)
    }

    pub(super) fn rebuild_admission_snapshot_for_input(
        &self,
        input: &super::owner::InputPermit,
    ) -> Result<FundedSnapshot, OperationJournalStoreError> {
        self.rebuild_admission_snapshot_with_funding(Some(input))
    }

    fn rebuild_admission_snapshot_with_funding(
        &self,
        input: Option<&super::owner::InputPermit>,
    ) -> Result<FundedSnapshot, OperationJournalStoreError> {
        self.validate_namespace()?;
        self.connection.execute_batch("BEGIN")?;
        // Declare credit before output/working backing so error/unwind drops backing first.
        let mut collection = None;
        let result = (|| {
            let snapshot =
                self.rebuild_ordinary_claim_snapshot_with_funding(input, &mut collection)?;
            self.validate_chunk_claims_inside_transaction()?;
            match input {
                Some(input) => recovery_reserve::prove_no_active_charges_for_input(
                    &self.connection,
                    &self.database_path,
                    input,
                )?,
                None => recovery_reserve::prove_no_active_charges(&self.connection).map_err(
                    |error| {
                        audit_error(
                            &self.database_path,
                            format!("recovery reserve admission audit failed: {error}"),
                        )
                    },
                )?,
            }
            self.validate_namespace_for_precommit()?;
            Ok(snapshot)
        })();
        match result {
            Ok(snapshot) => {
                self.connection.execute_batch("ROLLBACK")?;
                self.validate_namespace()?;
                Ok(FundedSnapshot::new(
                    snapshot,
                    collection.map(admission_rebuild_backing::Collection::into_credit),
                ))
            }
            Err(error) => {
                let _ = self.connection.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    /// Validate one prepared initial-intent record without touching the journal connection.
    /// Callers use this before allocating a real operation identity or reserving capacity.
    pub(crate) fn validate_initial_intent_record(
        record: &StoredOperationRecord,
    ) -> Result<(), OperationJournalStoreError> {
        validate_intent_record(record)?;
        encode(record)
            .map(|_| ())
            .map_err(OperationJournalStoreError::from)
    }

    /// Read one durable post-intent chunk claim. The row is an observation; issuance and release
    /// remain owner-thread transitions below.
    pub(crate) fn read_chunk_claim(
        &self,
        chunk_id: chunk_claims::ChunkId,
    ) -> Result<Option<chunk_claims::ChunkClaimRecord>, OperationJournalStoreError> {
        self.validate_namespace()?;
        chunk_claims::read(&self.connection, chunk_id).map_err(|error| {
            OperationJournalStoreError::ChunkClaim {
                reason: error.to_string(),
            }
        })
    }

    /// Atomically append one ordinary claim and its post-intent chunk relation. Both the global
    /// chunk relation and each operation's ordinary claim image are bounded by
    /// [`ordinary_claims::MAX_PAGE_ROWS`], so restart validation never has to scan unbounded
    /// post-intent history. The lease is pinned before the transaction; a confirmed rollback
    /// releases it, while any uncertain or committed-but-unreadable boundary leaves the
    /// provider charge retained.
    pub(crate) fn persist_chunk_claim_with_lease(
        &self,
        request: &chunk_claims::ChunkClaimRequest,
        provider: &CapacityProvider,
        mut lease: ProvisionalCapacityLease,
    ) -> Result<chunk_claims::ChunkClaimRecord, OperationJournalStoreError> {
        request
            .validate()
            .map_err(|error| OperationJournalStoreError::ChunkClaim {
                reason: error.to_string(),
            })?;
        let Some(operation) = self.get(request.operation_id())? else {
            return Err(OperationJournalStoreError::OperationNotFound {
                operation_id: request.operation_id(),
            });
        };
        if operation.phase != OperationPhase::IntentDurable
            || !matches!(
                operation.disposition,
                OperationDisposition::None | OperationDisposition::RetryPending
            )
        {
            return Err(OperationJournalStoreError::NotIntentDurable {
                phase: operation.phase,
            });
        }
        let Some(universal) = operation.universal_intent.as_ref() else {
            return Err(OperationJournalStoreError::ChunkClaim {
                reason: String::from("chunk claim operation has no participant envelope"),
            });
        };
        if !universal
            .participants()
            .descriptors()
            .iter()
            .any(|participant| participant.id() == request.participant_id())
        {
            return Err(OperationJournalStoreError::ChunkClaim {
                reason: String::from("chunk claim participant is not in the operation envelope"),
            });
        }
        let plan = request
            .plan()
            .map_err(|error| OperationJournalStoreError::ChunkClaim {
                reason: error.to_string(),
            })?;
        let claims =
            request
                .inventory()
                .map_err(|error| OperationJournalStoreError::ChunkClaim {
                    reason: error.to_string(),
                })?;
        ordinary_claims::validate_storage_values(&claims).map_err(|error| {
            OperationJournalStoreError::ChunkClaim {
                reason: error.to_string(),
            }
        })?;
        lease
            .validate_against(provider, &plan, &claims)
            .map_err(|error| OperationJournalStoreError::ChunkClaim {
                reason: error.to_string(),
            })?;
        lease
            .pin_for_persistence(request.operation_id())
            .map_err(|error| OperationJournalStoreError::ChunkClaim {
                reason: error.to_string(),
            })?;

        self.validate_namespace()?;
        self.connection.execute_batch("BEGIN")?;
        let transaction_result = (|| {
            let operation = self
                .read_row(request.operation_id())?
                .ok_or(OperationJournalStoreError::OperationNotFound {
                    operation_id: request.operation_id(),
                })?
                .record;
            if operation.phase != OperationPhase::IntentDurable
                || !matches!(
                    operation.disposition,
                    OperationDisposition::None | OperationDisposition::RetryPending
                )
            {
                return Err(OperationJournalStoreError::ChunkClaim {
                    reason: format!(
                        "operation {} is not eligible for a post-intent chunk claim",
                        request.operation_id()
                    ),
                });
            }
            // A new relation row must leave the resulting image within the same bounded
            // restart audit.  Asking whether the current image exceeds MAX-1 rejects both an
            // already-over-limit image and the at-limit append, while still allowing the
            // 256th row.
            if chunk_claims::exceeds_row_limit(
                &self.connection,
                chunk_claims::MAX_PAGE_ROWS.saturating_sub(1),
            )
            .map_err(|error| OperationJournalStoreError::ChunkClaim {
                reason: error.to_string(),
            })? {
                return Err(OperationJournalStoreError::ChunkClaim {
                    reason: format!(
                        "chunk-claim relation already has the bounded {} row limit",
                        chunk_claims::MAX_PAGE_ROWS
                    ),
                });
            }
            let mut count_statement = self.connection.prepare(&format!(
                "SELECT claim_id FROM {} WHERE operation_id = ?1 LIMIT ?2",
                ordinary_claims::TABLE_NAME
            ))?;
            let mut count_rows = count_statement.query(params![
                request.operation_id().to_string(),
                i64::try_from(ordinary_claims::MAX_PAGE_ROWS).expect("bounded claim count"),
            ])?;
            let mut claim_count = 0usize;
            while count_rows.next()?.is_some() {
                claim_count = claim_count.saturating_add(1);
                if claim_count >= ordinary_claims::MAX_PAGE_ROWS {
                    break;
                }
            }
            if claim_count >= ordinary_claims::MAX_PAGE_ROWS {
                return Err(OperationJournalStoreError::ChunkClaim {
                    reason: format!(
                        "operation already has the maximum {} ordinary claims",
                        ordinary_claims::MAX_PAGE_ROWS
                    ),
                });
            }
            if chunk_claims::read(&self.connection, request.chunk_id())
                .map_err(|error| OperationJournalStoreError::ChunkClaim {
                    reason: error.to_string(),
                })?
                .is_some()
            {
                return Err(OperationJournalStoreError::ChunkClaim {
                    reason: String::from("chunk identity already exists"),
                });
            }
            ordinary_claims::insert_rows_inside_transaction(
                &self.connection,
                request.operation_id(),
                &claims,
            )
            .map_err(|error| OperationJournalStoreError::ChunkClaim {
                reason: error.to_string(),
            })?;
            chunk_claims::insert_claimed_inside_transaction(&self.connection, request).map_err(
                |error| OperationJournalStoreError::ChunkClaim {
                    reason: error.to_string(),
                },
            )?;
            #[cfg(test)]
            if let Some(directive) =
                self.insert_test_directive(InsertTestBoundary::BeforePrecommitValidation)
            {
                match directive {
                    InsertTestDirective::Fail(error)
                    | InsertTestDirective::FailAndLeaveForRollback(error) => return Err(error),
                    InsertTestDirective::Continue | InsertTestDirective::ExecuteThenFail(_) => {}
                }
            }
            self.validate_namespace_for_precommit()?;
            Ok::<(), OperationJournalStoreError>(())
        })();
        if let Err(error) = transaction_result {
            let _ = self.connection.execute_batch("ROLLBACK");
            if let Err(release_error) =
                lease.release_after_confirmed_rollback(request.operation_id())
            {
                return Err(OperationJournalStoreError::ChunkClaim {
                    reason: format!(
                        "{error}; confirmed rollback could not release charge: {release_error}"
                    ),
                });
            }
            return Err(error);
        }
        #[cfg(test)]
        let commit_directive = self.insert_test_directive(InsertTestBoundary::Commit);
        #[cfg(test)]
        if let Some(InsertTestDirective::Fail(error)) = commit_directive {
            let rollback_error = self.connection.execute_batch("ROLLBACK").err();
            if rollback_error.is_none() {
                if let Err(release_error) =
                    lease.release_after_confirmed_rollback(request.operation_id())
                {
                    return Err(OperationJournalStoreError::ChunkClaim {
                        reason: format!(
                            "{error}; confirmed chunk-claim rollback could not release charge: {release_error}"
                        ),
                    });
                }
            }
            return Err(OperationJournalStoreError::ChunkClaim {
                reason: format!("chunk claim commit was rejected before acknowledgement: {error}"),
            });
        }
        #[cfg(test)]
        if let Some(InsertTestDirective::ExecuteThenFail(error)) = commit_directive {
            let commit_result = self.connection.execute_batch("COMMIT");
            let rollback_error = self.connection.execute_batch("ROLLBACK").err();
            return Err(OperationJournalStoreError::ChunkClaim {
                reason: format!(
                    "chunk claim commit outcome is uncertain: {error}; commit={commit_result:?}; rollback={rollback_error:?}"
                ),
            });
        }
        if let Err(error) = self.connection.execute_batch("COMMIT") {
            return Err(OperationJournalStoreError::ChunkClaim {
                reason: format!("chunk claim commit outcome is uncertain: {error}"),
            });
        }
        #[cfg(test)]
        if let Some(InsertTestDirective::Fail(error)) =
            self.insert_test_directive(InsertTestBoundary::AfterCommit)
        {
            return Err(OperationJournalStoreError::ChunkClaim {
                reason: format!("chunk claim committed but acknowledgement failed: {error}"),
            });
        }
        if let Err(error) = self.validate_namespace() {
            return Err(OperationJournalStoreError::ChunkClaim {
                reason: format!(
                    "chunk claim committed but namespace acknowledgement failed: {error}"
                ),
            });
        }
        lease
            .mark_durable(request.operation_id())
            .map_err(|error| OperationJournalStoreError::ChunkClaim {
                reason: format!("chunk claim committed but provider settlement failed: {error}"),
            })?;
        self.read_chunk_claim(request.chunk_id())?
            .filter(|record| record.matches_request(request))
            .ok_or_else(|| OperationJournalStoreError::ChunkClaim {
                reason: String::from("chunk claim committed but exact readback failed"),
            })
    }

    /// Persist the Claimed-to-Issued transition and return the exact durable row. Replays of an
    /// already-issued or released row are read-only and never mint a second transition.
    pub(crate) fn issue_chunk_claim(
        &self,
        chunk_id: chunk_claims::ChunkId,
        operation_id: OperationId,
        issue_id: Uuid,
    ) -> Result<chunk_claims::ChunkIssueReceipt, OperationJournalStoreError> {
        self.validate_namespace()?;
        self.connection.execute_batch("BEGIN")?;
        let result = (|| {
            let operation = self
                .read_row(operation_id)?
                .ok_or(OperationJournalStoreError::OperationNotFound { operation_id })?
                .record;
            if operation.phase != OperationPhase::IntentDurable
                || !matches!(
                    operation.disposition,
                    OperationDisposition::None | OperationDisposition::RetryPending
                )
            {
                return Err(OperationJournalStoreError::ChunkClaim {
                    reason: format!("operation {operation_id} is not eligible for chunk issuance"),
                });
            }
            Ok(chunk_claims::mark_issued_inside_transaction(
                &self.connection,
                chunk_id,
                operation_id,
                issue_id,
            )
            .map_err(|error| OperationJournalStoreError::ChunkClaim {
                reason: error.to_string(),
            })?)
        })()
        .map_err(|error| OperationJournalStoreError::ChunkClaim {
            reason: error.to_string(),
        })
        .and_then(|record| {
            let record = record.ok_or_else(|| OperationJournalStoreError::ChunkClaim {
                reason: String::from(
                    "chunk issuance transition was already consumed; no issue receipt minted",
                ),
            })?;
            #[cfg(test)]
            if let Some(directive) =
                self.insert_test_directive(InsertTestBoundary::BeforePrecommitValidation)
            {
                match directive {
                    InsertTestDirective::Fail(error)
                    | InsertTestDirective::FailAndLeaveForRollback(error) => return Err(error),
                    InsertTestDirective::Continue | InsertTestDirective::ExecuteThenFail(_) => {}
                }
            }
            self.validate_namespace_for_precommit()?;
            Ok(record)
        });
        if let Err(error) = result {
            let _ = self.connection.execute_batch("ROLLBACK");
            return Err(error);
        }
        #[cfg(test)]
        let commit_directive = self.insert_test_directive(InsertTestBoundary::Commit);
        #[cfg(test)]
        if let Some(InsertTestDirective::Fail(error)) = commit_directive {
            let _ = self.connection.execute_batch("ROLLBACK");
            return Err(OperationJournalStoreError::ChunkClaim {
                reason: format!("chunk issuance commit was rejected: {error}"),
            });
        }
        #[cfg(test)]
        if let Some(InsertTestDirective::ExecuteThenFail(error)) = commit_directive {
            let commit_result = self.connection.execute_batch("COMMIT");
            let rollback_error = self.connection.execute_batch("ROLLBACK").err();
            return Err(OperationJournalStoreError::ChunkClaim {
                reason: format!(
                    "chunk issuance commit outcome is uncertain: {error}; commit={commit_result:?}; rollback={rollback_error:?}"
                ),
            });
        }
        self.connection.execute_batch("COMMIT")?;
        #[cfg(test)]
        if let Some(InsertTestDirective::Fail(error)) =
            self.insert_test_directive(InsertTestBoundary::AfterCommit)
        {
            return Err(OperationJournalStoreError::ChunkClaim {
                reason: format!("chunk issuance committed but acknowledgement failed: {error}"),
            });
        }
        self.validate_namespace()?;
        let record = self.read_chunk_claim(chunk_id)?.ok_or_else(|| {
            OperationJournalStoreError::ChunkClaim {
                reason: String::from("issued chunk claim disappeared during readback"),
            }
        })?;
        chunk_claims::ChunkIssueReceipt::from_record(record).ok_or_else(|| {
            OperationJournalStoreError::ChunkClaim {
                reason: String::from("issued chunk readback did not contain a sealed issue proof"),
            }
        })
    }

    /// Durably release a chunk claim and its ordinary row. Provider accounting is reconciled by
    /// the owner only after this transaction has committed and read back the exact receipt.
    pub(crate) fn release_chunk_claim(
        &self,
        chunk_id: chunk_claims::ChunkId,
        operation_id: OperationId,
        release_authorization: Option<ChunkUnusedReleaseAuthorization>,
        release_id: Uuid,
    ) -> Result<OrdinaryClaimReleaseReceipt, OperationJournalStoreError> {
        if release_id.is_nil() {
            return Err(OperationJournalStoreError::ChunkClaim {
                reason: String::from("release identity is nil"),
            });
        }
        self.validate_namespace()?;
        self.connection.execute_batch("BEGIN")?;
        let result = (|| {
            let current = chunk_claims::read(&self.connection, chunk_id)
                .map_err(|error| OperationJournalStoreError::ChunkClaim {
                    reason: error.to_string(),
                })?
                .ok_or_else(|| OperationJournalStoreError::ChunkClaim {
                    reason: String::from("chunk claim was not found"),
                })?;
            if current.operation_id() != operation_id {
                return Err(OperationJournalStoreError::ChunkClaim {
                    reason: String::from("chunk claim belongs to a different operation"),
                });
            }
            let has_checkpoint: bool = self.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM operation_journal_chunk_staging WHERE chunk_id = ?1)
                      OR EXISTS(SELECT 1 FROM operation_journal_recording_chunks WHERE operation_id = ?2)",
                params![chunk_id.to_string(), operation_id.to_string()],
                |row| row.get(0),
            )?;
            if has_checkpoint {
                return Err(OperationJournalStoreError::ChunkClaim {
                    reason: String::from("incomplete chunk staging checkpoint retains its charge"),
                });
            }
            let claim = ordinary_claims::read_claim(&self.connection, current.claim_id())
                .map_err(|error| OperationJournalStoreError::ChunkClaim {
                    reason: error.to_string(),
                })?
                .ok_or_else(|| OperationJournalStoreError::ChunkClaim {
                    reason: String::from("chunk claim has no ordinary claim row"),
                })?;
            if claim.operation_id() != operation_id
                || claim.volume() != current.volume()
                || claim.allocation_class() != current.allocation_class()
                || claim.allocated_bytes() != current.allocated_bytes()
            {
                return Err(OperationJournalStoreError::ChunkClaim {
                    reason: String::from("chunk and ordinary claim metadata differ"),
                });
            }
            if current.state() == chunk_claims::ChunkClaimState::Released {
                if current.release_id() != Some(release_id) {
                    return Err(OperationJournalStoreError::ChunkClaim {
                        reason: String::from(
                            "chunk release identity conflicts with durable release",
                        ),
                    });
                }
                let release = ordinary_claims::read_release(&self.connection, current.claim_id())
                    .map_err(|error| OperationJournalStoreError::ChunkClaim {
                        reason: error.to_string(),
                    })?
                    .ok_or_else(|| OperationJournalStoreError::ChunkClaim {
                        reason: String::from("released chunk is missing ordinary release evidence"),
                    })?;
                if release.operation_id() != operation_id || release.release_id() != release_id {
                    return Err(OperationJournalStoreError::ChunkClaim {
                        reason: String::from(
                            "ordinary release evidence conflicts with chunk state",
                        ),
                    });
                }
                self.validate_namespace_for_precommit()?;
                return Ok(OrdinaryClaimReleaseReceipt::from_claim(&claim, release_id));
            }
            if current.state() == chunk_claims::ChunkClaimState::Issued {
                let Some(authorization) = release_authorization.as_ref() else {
                    return Err(OperationJournalStoreError::ChunkClaim {
                        reason: String::from(
                            "issued chunk release requires a sealed owner authorization",
                        ),
                    });
                };
                if authorization.chunk_id() != chunk_id
                    || authorization.operation_id() != operation_id
                    || authorization.issue_id() != current.issue_id().unwrap_or_else(Uuid::nil)
                    || authorization.release_id() != release_id
                {
                    return Err(OperationJournalStoreError::ChunkClaim {
                        reason: String::from("issued chunk release proof does not match"),
                    });
                }
            } else if release_authorization.is_some() {
                return Err(OperationJournalStoreError::ChunkClaim {
                    reason: String::from("chunk release authorization is not valid for Claimed"),
                });
            }
            let updated = chunk_claims::mark_released_inside_transaction(
                &self.connection,
                chunk_id,
                operation_id,
                release_id,
            )
            .map_err(|error| OperationJournalStoreError::ChunkClaim {
                reason: error.to_string(),
            })?;
            let ordinary_release = ordinary_claims::OrdinaryClaimReleaseRecord::new(
                current.claim_id(),
                operation_id,
                release_id,
            );
            ordinary_claims::insert_release_inside_transaction(&self.connection, ordinary_release)
                .map_err(|error| OperationJournalStoreError::ChunkClaim {
                    reason: error.to_string(),
                })?;
            ordinary_claims::verify_release_inside_transaction(&self.connection, ordinary_release)
                .map_err(|error| OperationJournalStoreError::ChunkClaim {
                    reason: error.to_string(),
                })?;
            if updated.release_id() != Some(release_id) {
                return Err(OperationJournalStoreError::ChunkClaim {
                    reason: String::from("chunk release readback did not match"),
                });
            }
            #[cfg(test)]
            if let Some(directive) =
                self.release_test_directive(ReleaseTestBoundary::BeforePrecommitValidation)
            {
                match directive {
                    ReleaseTestDirective::Fail(error)
                    | ReleaseTestDirective::FailAndLeaveForRollback(error) => return Err(error),
                    ReleaseTestDirective::Continue | ReleaseTestDirective::ExecuteThenFail(_) => {}
                }
            }
            self.validate_namespace_for_precommit()?;
            Ok(OrdinaryClaimReleaseReceipt::from_claim(&claim, release_id))
        })();
        match result {
            Ok(receipt) => {
                #[cfg(test)]
                let commit_directive = self.release_test_directive(ReleaseTestBoundary::Commit);
                #[cfg(test)]
                if let Some(ReleaseTestDirective::Fail(error)) = commit_directive {
                    let _ = self.connection.execute_batch("ROLLBACK");
                    return Err(OperationJournalStoreError::ChunkClaim {
                        reason: format!("chunk release commit was rejected: {error}"),
                    });
                }
                #[cfg(test)]
                if let Some(ReleaseTestDirective::ExecuteThenFail(error)) = commit_directive {
                    let commit_result = self.connection.execute_batch("COMMIT");
                    let rollback_error = self.connection.execute_batch("ROLLBACK").err();
                    return Err(OperationJournalStoreError::ChunkClaim {
                        reason: format!(
                            "chunk release commit outcome is uncertain: {error}; commit={commit_result:?}; rollback={rollback_error:?}"
                        ),
                    });
                }
                if let Err(error) = self.connection.execute_batch("COMMIT") {
                    let _ = self.connection.execute_batch("ROLLBACK");
                    return Err(OperationJournalStoreError::ChunkClaim {
                        reason: format!("chunk release commit outcome is uncertain: {error}"),
                    });
                }
                #[cfg(test)]
                if let Some(ReleaseTestDirective::Fail(error)) =
                    self.release_test_directive(ReleaseTestBoundary::AfterCommit)
                {
                    return Err(OperationJournalStoreError::ChunkClaim {
                        reason: format!(
                            "chunk release committed but acknowledgement failed: {error}"
                        ),
                    });
                }
                self.validate_namespace()?;
                Ok(receipt)
            }
            Err(error) => {
                let _ = self.connection.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    /// Establish structured durable-zero evidence for the orderly volume-owner handoff.
    ///
    /// The ordinary scanner and recovery-reserve ledger reader each validate their complete
    /// bounded relation set.  Their user-facing readiness diagnostics are deliberately not used
    /// as proof; a valid empty/reconstituted history is accepted only through these typed checks.
    pub(crate) fn prove_clean_handoff_durable(&self) -> Result<(), OperationJournalStoreError> {
        self.validate_namespace()?;
        self.connection.execute_batch("BEGIN")?;
        let result = (|| {
            let ordinary = self.rebuild_ordinary_claim_snapshot_inside_transaction()?;
            self.validate_chunk_claims_inside_transaction()?;
            if !ordinary.proves_durable_zero() {
                return Err(audit_error(
                    &self.database_path,
                    "ordinary claim history retains an unreleased or ambiguous claim",
                ));
            }
            recovery_reserve::prove_no_active_charges(&self.connection).map_err(|error| {
                audit_error(
                    &self.database_path,
                    format!("recovery reserve history is not durably zero: {error}"),
                )
            })?;
            let staging_rows: i64 = self.connection.query_row(
                "SELECT (SELECT COUNT(*) FROM operation_journal_staging_checkpoints)
                      + (SELECT COUNT(*) FROM operation_journal_chunk_staging)
                      + (SELECT COUNT(*) FROM operation_journal_recording_chunks)
                      + (SELECT COUNT(*) FROM operation_journal_database_attempt)
                      + (SELECT COUNT(*) FROM operation_journal_database_attempt_outcome)
                      + (SELECT COUNT(*) FROM operation_journal_publication_issuance)",
                [],
                |row| row.get(0),
            )?;
            if staging_rows != 0 {
                return Err(audit_error(
                    &self.database_path,
                    "staging checkpoint evidence remains unresolved",
                ));
            }
            self.validate_namespace_for_precommit()?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.connection.execute_batch("ROLLBACK")?;
                self.validate_namespace()?;
                Ok(())
            }
            Err(error) => {
                let _ = self.connection.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    /// Validate the profile-owned journal boundary before a future admission decision.
    ///
    /// This intentionally performs no operation-ID allocation, persistence, capacity
    /// reservation, or authority construction. It only confirms that the actor still owns the
    /// expected journal namespace.
    pub(crate) fn validate_for_admission(&self) -> Result<(), OperationJournalStoreError> {
        self.validate_namespace()
    }

    pub(crate) fn try_clone_profile_guard_for_owner(
        &self,
    ) -> Result<WritableProfileGuard, ProfileOwnershipError> {
        self.profile_guard.try_clone()
    }

    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn test_reserve_bootstrap_snapshot(
        &self,
        volume: &crate::durable_capacity::VolumeIdentity,
    ) -> Result<(Option<(String, String, u64, u64)>, (u64, u64, u64, u64)), String> {
        let binding = recovery_reserve_binding::read(&self.connection, volume)
            .map_err(|error| error.to_string())?
            .map(|binding| {
                (
                    binding.root_identity().to_owned(),
                    binding.file_identity().to_owned(),
                    binding.allocation_unit(),
                    binding.owner_generation(),
                )
            });
        let count = |table: &str| {
            self.connection
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get::<_, i64>(0)
                })
                .map(|count| count as u64)
                .map_err(|error| error.to_string())
        };
        let operation_rows = count("operation_journal")?
            .checked_add(count("operation_journal_full_progress")?)
            .ok_or_else(|| String::from("operation row count overflowed"))?;
        let counts = (
            operation_rows,
            count(recovery_reserve_binding::TABLE_NAME)?,
            count(recovery_reserve::CHARGE_TABLE_NAME)?,
            count(recovery_reserve::TRANSITION_TABLE_NAME)?,
        );
        Ok((binding, counts))
    }

    /// Acquire the descriptor-derived shared volume owner after this store reaches its owner
    /// thread. The caller must install the returned capability before any later fallible reserve
    /// bootstrap so the owner runtime retains the lock across every error path.
    pub(crate) fn acquire_profile_journal_volume_owner(
        &self,
        #[cfg(any(test, feature = "test-support"))] registry_path: Option<&Path>,
    ) -> Result<VolumeOwnershipCapability, ProfileJournalReserveBootstrapError> {
        self.validate_namespace()?;
        let descriptor = self.database_root.try_clone_descriptor().map_err(|error| {
            ProfileJournalReserveBootstrapError::RootDescriptor(error.to_string())
        })?;
        #[cfg(any(test, feature = "test-support"))]
        {
            let volume_owner = match registry_path {
                Some(registry_path) => {
                    super::capacity_volume_owner::acquire_for_descriptor_fixture_test(
                        &descriptor,
                        registry_path,
                        &self.profile_guard,
                    )?
                }
                None => super::capacity_volume_owner::acquire_for_descriptor_after_clean_handoff(
                    &descriptor,
                    &self.profile_guard,
                )?,
            };
            return Ok(volume_owner);
        }
        #[cfg(not(any(test, feature = "test-support")))]
        super::capacity_volume_owner::acquire_for_descriptor_after_clean_handoff(
            &descriptor,
            &self.profile_guard,
        )
        .map_err(ProfileJournalReserveBootstrapError::from)
    }

    /// Bootstrap or reopen the profile/journal volume reserve using an already-installed owner
    /// lock. This is recovery-only state: it creates no operation id, charge, permit, or ordinary
    /// admission authority. Any context clone of the lock is dropped before this returns.
    pub(crate) fn bootstrap_profile_journal_reserve(
        &self,
        volume_owner: &VolumeOwnershipCapability,
        #[cfg(any(test, feature = "test-support"))] registry_path: Option<&Path>,
    ) -> Result<OwnedReserveAllocation, ProfileJournalReserveBootstrapError> {
        self.validate_namespace()?;
        #[cfg(any(test, feature = "test-support"))]
        let _ = registry_path;
        volume_owner.revalidate()?;

        let existing = recovery_reserve_binding::read(&self.connection, volume_owner.volume())?;
        if let Some(binding) = existing {
            if binding.owner_generation() != volume_owner.owner_generation() {
                return self.rebind_clean_profile_journal_reserve(volume_owner, binding);
            }
            let context_owner = volume_owner.try_clone_for_context()?;
            let context = self.reopen_recovery_reserve(
                context_owner,
                &self.database_root,
                &self.profile_guard,
            )?;
            return context
                .into_owned_reserve()
                .map_err(ProfileJournalReserveBootstrapError::from);
        }

        let reserve = super::capacity_reserve_namespace::create_owned_reserve(
            &self.database_root,
            &self.profile_guard,
        )
        .map_err(|error| ProfileJournalReserveBootstrapError::Namespace(error.to_string()))?;
        let mut reserve = reserve
            .into_owned()
            .map_err(|error| ProfileJournalReserveBootstrapError::Namespace(error.to_string()))?;
        {
            let view = reserve.view(&self.profile_guard);
            view.revalidate_and_prove().map_err(|error| {
                ProfileJournalReserveBootstrapError::NamespaceValidation(error.to_string())
            })?;
        }
        self.validate_namespace()?;
        volume_owner.revalidate()?;
        let observation = reserve.observation();
        let binding = recovery_reserve_binding::ReserveBinding::from_live(
            volume_owner.volume().clone(),
            self.profile_guard.profile_root_identity(),
            observation.root_identity(),
            observation.file_identity(),
            observation.allocation_unit(),
            volume_owner.owner_generation(),
        )?;
        recovery_reserve_binding::persist(&self.connection, &binding)?;
        Ok(reserve)
    }

    /// Recover a same-profile clean handoff whose marker acquisition advanced the owner
    /// generation.  The existing binding is opened and fully revalidated before its generation
    /// is changed; no reserve path or foreign profile artifact may be adopted.
    fn rebind_clean_profile_journal_reserve(
        &self,
        volume_owner: &VolumeOwnershipCapability,
        binding: recovery_reserve_binding::ReserveBinding,
    ) -> Result<OwnedReserveAllocation, ProfileJournalReserveBootstrapError> {
        let profile_identity = self.profile_guard.profile_root_identity();
        if binding.profile_identity() != profile_identity
            || binding.volume() != volume_owner.volume()
            || binding.root_identity() != self.database_root.identity()
            || binding.namespace_version() != recovery_reserve_binding::NAMESPACE_VERSION
            || binding.namespace_name() != super::capacity_reserve_namespace::RESERVE_FILE_NAME
            || binding.protected_floor_bytes()
                != crate::durable_capacity::PROTECTED_FREE_SPACE_FLOOR
        {
            return Err(ProfileJournalReserveBootstrapError::Binding(
                recovery_reserve_binding::ReserveBindingError::IdentityMismatch(
                    "clean handoff binding does not match the current profile/root namespace"
                        .into(),
                ),
            ));
        }
        if binding.owner_generation() >= volume_owner.owner_generation() {
            return Err(ProfileJournalReserveBootstrapError::Binding(
                recovery_reserve_binding::ReserveBindingError::IdentityMismatch(
                    "stored owner generation is not older than the live clean-marker generation"
                        .into(),
                ),
            ));
        }
        self.prove_clean_handoff_durable()?;
        volume_owner.revalidate()?;
        let expected =
            super::capacity_reserve_namespace::ReserveNamespaceObservation::from_binding(
                binding.root_identity(),
                binding.file_identity(),
                binding.volume().clone(),
                binding.allocation_unit(),
            )
            .map_err(|reason| ProfileJournalReserveBootstrapError::Namespace(reason.into()))?;
        let reserve = super::capacity_reserve_namespace::reopen_owned_reserve(
            &self.database_root,
            &self.profile_guard,
            &expected,
        )
        .map_err(|error| ProfileJournalReserveBootstrapError::Namespace(error.to_string()))?;
        let mut reserve = reserve
            .into_owned()
            .map_err(|error| ProfileJournalReserveBootstrapError::Namespace(error.to_string()))?;
        let proof = reserve
            .view(&self.profile_guard)
            .revalidate_and_prove()
            .map_err(|error| ProfileJournalReserveBootstrapError::Namespace(error.to_string()))?;
        if proof.volume() != volume_owner.volume()
            || !proof.non_sparse()
            || proof.physical_bytes() < crate::durable_capacity::PROTECTED_FREE_SPACE_FLOOR
        {
            return Err(ProfileJournalReserveBootstrapError::Namespace(
                "clean handoff reserve proof does not meet the protected floor".into(),
            ));
        }
        volume_owner.revalidate()?;
        recovery_reserve_binding::rebind_owner_generation(
            &self.connection,
            &binding,
            volume_owner.owner_generation(),
        )?;
        volume_owner.revalidate()?;
        Ok(reserve)
    }

    /// Return the physical reserve extent required by the current durable charge state.
    ///
    /// A fully allocated reserve uses the protected floor. A valid active charge may have
    /// reconstituted only the remaining extent, so owner startup uses this same minimum when it
    /// checks a fresh physical proof before rebuilding ordinary claims.
    pub(crate) fn minimum_recovery_reserve_bytes(
        &self,
        volume: &VolumeIdentity,
    ) -> Result<u64, String> {
        self.validate_recording_prestart_namespace()
            .map_err(|error| error.to_string())?;
        let active = recovery_reserve::active_charge_snapshot_for_volume(&self.connection, volume)
            .map_err(|error| error.to_string())?;
        let Some(snapshot) = active else {
            return Ok(crate::durable_capacity::PROTECTED_FREE_SPACE_FLOOR);
        };
        let spent = snapshot
            .budget_bytes
            .checked_sub(snapshot.remaining_bytes)
            .ok_or_else(|| String::from("durable reserve remaining bytes exceed its budget"))?;
        let minimum = crate::durable_capacity::PROTECTED_FREE_SPACE_FLOOR
            .checked_sub(spent)
            .ok_or_else(|| String::from("durable reserve spend exceeds the protected floor"))?;
        if minimum < snapshot.margin_bytes {
            return Err(String::from(
                "durable reserve spend consumes its control margin",
            ));
        }
        Ok(minimum)
    }

    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn test_seed_spent_profile_journal_reserve(
        &self,
        registry_path: &Path,
        operation_id: OperationId,
        charge_id: uuid::Uuid,
        budget_bytes: u64,
        margin_bytes: u64,
        spend_bytes: u64,
    ) -> Result<(), String> {
        let descriptor = self
            .database_root
            .try_clone_descriptor()
            .map_err(|error| error.to_string())?;
        let facts = crate::durable_capacity::descriptor_capacity_facts(&descriptor)
            .map_err(|error| error.to_string())?;
        let volume_owner = super::capacity_volume_owner::acquire_fixture_for_test(
            &descriptor,
            registry_path,
            facts.identity.clone(),
            self.profile_guard.profile_root_identity(),
        )
        .map_err(|error| error.to_string())?;
        let reserve = super::capacity_reserve_namespace::create_owned_reserve(
            &self.database_root,
            &self.profile_guard,
        )
        .map_err(|error| error.to_string())?;
        let mut context = self
            .bind_recovery_reserve(volume_owner, reserve)
            .map_err(|error| error.to_string())?;
        let request = recovery_reserve::ChargeRequest {
            charge_id,
            operation_id,
            volume: facts.identity,
            purpose: recovery_reserve::RecoveryReservePurpose::OperationRecovery,
            budget_bytes,
            margin_bytes,
            precharge_capacity_bytes: facts.free_bytes,
        };
        context
            .begin_charge(&request)
            .map_err(|error| error.to_string())?;
        context
            .spend_physical(charge_id, uuid::Uuid::new_v4(), spend_bytes)
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// Bind the durable ledger to a live per-volume owner and physical reserve.
    ///
    /// This private recovery path validates every retained descriptor before returning. It does
    /// not enable ordinary admission, allocate an operation id, or expose the SQLite connection.
    #[allow(dead_code)]
    pub(crate) fn bind_recovery_reserve<'a>(
        &'a self,
        volume_owner: VolumeOwnershipCapability,
        reserve: BoundReserve<'a>,
    ) -> Result<RecoveryReserveContext<'a>, RecoveryReserveContextError> {
        self.validate_namespace()?;
        let profile_identity = self.profile_guard.profile_root_identity().to_owned();
        if volume_owner.owner_profile_identity() != profile_identity
            || reserve.profile_identity() != profile_identity
        {
            return Err(RecoveryReserveContextError::ProfileIdentityMismatch);
        }
        if volume_owner.volume() != reserve.observation().volume() {
            return Err(RecoveryReserveContextError::VolumeIdentityMismatch);
        }
        volume_owner.revalidate()?;
        reserve
            .revalidate()
            .map_err(|error| RecoveryReserveContextError::Reserve(error.to_string()))?;
        let binding = recovery_reserve_binding::ReserveBinding::from_live(
            volume_owner.volume().clone(),
            &profile_identity,
            reserve.observation().root_identity(),
            reserve.observation().file_identity(),
            reserve.observation().allocation_unit(),
            volume_owner.owner_generation(),
        )?;
        recovery_reserve_binding::persist(&self.connection, &binding)?;
        Ok(RecoveryReserveContext {
            store: self,
            volume_owner,
            reserve,
            journal_identity: self.database_identity.clone(),
            profile_identity,
        })
    }

    /// Bind or reopen one owner-thread reserve for a descriptor-derived source volume.
    ///
    /// The returned ownership lock and reserve remain paired with the caller. Existing durable
    /// bindings are reopened through the same identity and active-charge checks as journal
    /// startup; a new binding is persisted only after a live reserve proof succeeds.
    /// Actual Current bootstrap accepts only the privately correlated original descriptor scope.
    /// Its failure sink is installed in the source-volume actor before any effect.
    pub(super) fn acquire_owned_current_reserve(
        &self,
        volume_owner: VolumeOwnershipCapability,
        proof: &crate::sample_sources::db::CurrentReserveUseProof<'_>,
        failure: &LegacyReserveFailureSink,
    ) -> Result<OwnedReserveAllocation, SourceReserveBindingFailure> {
        let _custody = match proof.begin_bootstrap_effect() {
            Ok(custody) => custody,
            Err(error) => {
                return Err(SourceReserveBindingFailure {
                    error: RecoveryReserveContextError::Reserve(error.to_string()),
                    reserve: None,
                    volume_owner,
                });
            }
        };
        let retained_owner = match volume_owner.try_clone_for_context() {
            Ok(owner) => owner,
            Err(error) => {
                return Err(SourceReserveBindingFailure {
                    error: error.into(),
                    reserve: None,
                    volume_owner,
                });
            }
        };
        let bootstrap = ReserveBootstrapProof::ScopedCurrent {
            source: proof,
            failure,
        };
        let reserve = OwnedCustody::new(
            self.acquire_owned_source_reserve_inner(
                volume_owner,
                proof.root(),
                proof.profile(),
                &bootstrap,
                &mut None,
            )?,
            &bootstrap,
        );
        if let Err(error) = proof.validate_root_for_bootstrap(proof.root()) {
            // The worker still owns the original volume capability; returning this reserve in
            // the failure transfers its exact opened descriptor into its preinstalled slot.
            return Err(SourceReserveBindingFailure {
                error: RecoveryReserveContextError::Reserve(error.to_string()),
                reserve: Some(reserve.take()),
                volume_owner: retained_owner,
            });
        }
        Ok(reserve.take())
    }

    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn acquire_owned_source_reserve(
        &self,
        volume_owner: VolumeOwnershipCapability,
        root: &BoundDatabaseRoot,
        profile_guard: &WritableProfileGuard,
        proof: &SourceAuthorityUseProof,
    ) -> Result<OwnedReserveAllocation, SourceReserveBindingFailure> {
        let mut no_hook = None;
        self.acquire_owned_source_reserve_inner(
            volume_owner,
            root,
            profile_guard,
            &ReserveBootstrapProof::Current(proof),
            &mut no_hook,
        )
    }

    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn acquire_owned_source_reserve_with_test_hook<F>(
        &self,
        volume_owner: VolumeOwnershipCapability,
        root: &BoundDatabaseRoot,
        profile_guard: &WritableProfileGuard,
        proof: &SourceAuthorityUseProof,
        hook: &mut F,
    ) -> Result<OwnedReserveAllocation, SourceReserveBindingFailure>
    where
        F: FnMut(),
    {
        let mut callback: Option<&mut dyn FnMut()> = Some(hook);
        self.acquire_owned_source_reserve_inner(
            volume_owner,
            root,
            profile_guard,
            &ReserveBootstrapProof::Current(proof),
            &mut callback,
        )
    }

    /// Bootstrap only from the original retained pre-lease worker custody, never decoded data.
    pub(super) fn acquire_owned_legacy_reserve(
        &self,
        volume_owner: VolumeOwnershipCapability,
        root: &BoundDatabaseRoot,
        profile_guard: &WritableProfileGuard,
        proof: &crate::sample_sources::db::LegacyReserveUseProof<'_>,
    ) -> Result<OwnedReserveAllocation, LegacyReserveBindingFailure> {
        let failure = LegacyReserveFailureSink::default();
        let result = self.acquire_owned_source_reserve_inner(
            volume_owner,
            root,
            profile_guard,
            &ReserveBootstrapProof::Legacy {
                source: proof,
                failure: &failure,
            },
            &mut None,
        );
        result.map_err(|original| LegacyReserveBindingFailure {
            original,
            namespace: failure.namespace.into_inner(),
            reopened_reserve: failure.reserve.into_inner(),
        })
    }

    #[cfg(all(test, unix))]
    pub(super) fn set_current_reopen_fixture(&self) {
        self.current_reopen_fixture.set(true);
    }
    #[cfg(all(test, unix))]
    pub(super) fn current_reopen_fixture(&self) -> bool {
        self.current_reopen_fixture.get()
    }

    #[cfg(all(test, unix))]
    pub(super) fn current_binding_generation_for_test(
        &self,
        volume: &VolumeIdentity,
    ) -> Option<u64> {
        recovery_reserve_binding::read(&self.connection, volume)
            .unwrap()
            .map(|b| b.owner_generation())
    }

    #[cfg(all(test, unix))]
    pub(super) fn set_current_reserve_test_hook(
        &self,
        hook: impl FnMut(CurrentReserveBoundary, &File) + Send + 'static,
    ) {
        *self.current_reserve_test_hook.lock().unwrap() = Some(Box::new(hook));
    }

    #[cfg(test)]
    fn notify_current_reserve_test_boundary(
        &self,
        proof: &ReserveBootstrapProof<'_, '_>,
        boundary: CurrentReserveBoundary,
        file: &File,
    ) {
        if !matches!(proof, ReserveBootstrapProof::ScopedCurrent { .. }) {
            return;
        }
        let hook = self.current_reserve_test_hook.lock().unwrap().take();
        if let Some(mut hook) = hook {
            hook(boundary, file);
            *self.current_reserve_test_hook.lock().unwrap() = Some(hook);
        }
    }

    fn acquire_owned_source_reserve_inner(
        &self,
        volume_owner: VolumeOwnershipCapability,
        root: &BoundDatabaseRoot,
        profile_guard: &WritableProfileGuard,
        proof: &ReserveBootstrapProof<'_, '_>,
        after_reserve: &mut Option<&mut dyn FnMut()>,
    ) -> Result<OwnedReserveAllocation, SourceReserveBindingFailure> {
        let fail = |error, volume_owner, reserve| SourceReserveBindingFailure {
            error,
            volume_owner,
            reserve,
        };
        let validate_proof = || {
            proof
                .validate_for_physical_use()
                .map_err(|error| error.to_string())
        };
        if let Err(error) = validate_proof() {
            return Err(fail(
                RecoveryReserveContextError::Reserve(format!(
                    "source authority proof is no longer valid: {error}"
                )),
                volume_owner,
                None,
            ));
        }
        if let Err(error) = self.validate_namespace() {
            return Err(fail(
                RecoveryReserveContextError::Store(error),
                volume_owner,
                None,
            ));
        }
        if let Err(error) = root
            .validate_profile_guard(profile_guard)
            .map_err(OperationJournalStoreError::ProfileOwnership)
        {
            return Err(fail(error.into(), volume_owner, None));
        }
        if let Err(error) = volume_owner.revalidate() {
            return Err(fail(error.into(), volume_owner, None));
        }
        let binding = match recovery_reserve_binding::read(&self.connection, volume_owner.volume())
        {
            Ok(binding) => binding,
            Err(error) => return Err(fail(error.into(), volume_owner, None)),
        };
        if binding.is_some() {
            if let Err(error) = validate_proof() {
                return Err(fail(
                    RecoveryReserveContextError::Reserve(format!(
                        "source authority proof is no longer valid: {error}"
                    )),
                    volume_owner,
                    None,
                ));
            }
            if binding.as_ref().is_some_and(|binding| {
                binding.owner_generation() != volume_owner.owner_generation()
            }) {
                let binding = binding.expect("source reserve binding was checked above");
                return match self.rebind_clean_source_reserve(
                    &volume_owner,
                    root,
                    profile_guard,
                    binding,
                    proof,
                ) {
                    Ok(reserve) => Ok(reserve),
                    Err(error) => Err(fail(error, volume_owner, None)),
                };
            }
            let context_owner = match volume_owner.try_clone_for_context() {
                Ok(owner) => owner,
                Err(error) => return Err(fail(error.into(), volume_owner, None)),
            };
            let context = match self.reopen_recovery_reserve_with_failure_proof(
                context_owner,
                root,
                profile_guard,
                Some(proof),
            ) {
                Ok(context) => context,
                Err(error) => return Err(fail(error, volume_owner, None)),
            };
            let RecoveryReserveContext { reserve: bound, .. } = context;
            return match bound
                .into_owned()
                .map_err(|error| proof.namespace_error(error))
            {
                Ok(reserve) => Ok(reserve),
                Err(error) => Err(fail(error, volume_owner, None)),
            };
        }

        if let Err(error) = validate_proof() {
            return Err(fail(
                RecoveryReserveContextError::Reserve(format!(
                    "source authority proof is no longer valid: {error}"
                )),
                volume_owner,
                None,
            ));
        }
        let reserve = match proof.create_reserve(root, profile_guard) {
            Ok(reserve) => reserve,
            Err(error) => {
                return Err(fail(proof.namespace_error(error), volume_owner, None));
            }
        };
        let reserve = match reserve.into_owned() {
            Ok(reserve) => reserve,
            Err(error) => {
                return Err(fail(proof.namespace_error(error), volume_owner, None));
            }
        };
        let mut reserve = OwnedCustody::new(reserve, proof);
        #[cfg(test)]
        self.notify_current_reserve_test_boundary(
            proof,
            CurrentReserveBoundary::Created,
            reserve.file_for_custody_test(),
        );
        if let Some(hook) = after_reserve.as_mut() {
            hook();
        }
        if let Err(error) = reserve
            .view(profile_guard)
            .revalidate_and_prove()
            .map_err(|error| RecoveryReserveContextError::Reserve(error.to_string()))
        {
            return Err(fail(error, volume_owner, Some(reserve.take())));
        }
        if let Err(error) = self.validate_namespace() {
            return Err(fail(
                RecoveryReserveContextError::Store(error),
                volume_owner,
                Some(reserve.take()),
            ));
        }
        if let Err(error) = volume_owner.revalidate() {
            return Err(fail(error.into(), volume_owner, Some(reserve.take())));
        }
        if let Err(error) = validate_proof() {
            return Err(fail(
                RecoveryReserveContextError::Reserve(format!(
                    "source authority proof is no longer valid: {error}"
                )),
                volume_owner,
                Some(reserve.take()),
            ));
        }
        let observation = reserve.observation();
        let binding = match recovery_reserve_binding::ReserveBinding::from_live(
            volume_owner.volume().clone(),
            self.profile_guard.profile_root_identity(),
            observation.root_identity(),
            observation.file_identity(),
            observation.allocation_unit(),
            volume_owner.owner_generation(),
        ) {
            Ok(binding) => binding,
            Err(error) => return Err(fail(error.into(), volume_owner, Some(reserve.take()))),
        };
        let effect = match proof.begin_effect() {
            Ok(effect) => effect,
            Err(error) => return Err(fail(error, volume_owner, Some(reserve.take()))),
        };
        if let Err(error) = recovery_reserve_binding::persist(&self.connection, &binding) {
            return Err(fail(error.into(), volume_owner, Some(reserve.take())));
        }
        drop(effect);
        #[cfg(test)]
        self.notify_current_reserve_test_boundary(
            proof,
            CurrentReserveBoundary::CreatedBound,
            reserve.file_for_custody_test(),
        );
        if let Err(error) = validate_proof() {
            return Err(fail(
                RecoveryReserveContextError::Reserve(error),
                volume_owner,
                Some(reserve.take()),
            ));
        }
        Ok(reserve.take())
    }

    fn rebind_clean_source_reserve(
        &self,
        volume_owner: &VolumeOwnershipCapability,
        root: &BoundDatabaseRoot,
        profile_guard: &WritableProfileGuard,
        binding: recovery_reserve_binding::ReserveBinding,
        proof: &ReserveBootstrapProof<'_, '_>,
    ) -> Result<OwnedReserveAllocation, RecoveryReserveContextError> {
        if binding.profile_identity() != profile_guard.profile_root_identity()
            || binding.volume() != volume_owner.volume()
            || binding.root_identity() != root.identity()
            || binding.namespace_version() != recovery_reserve_binding::NAMESPACE_VERSION
            || binding.namespace_name() != super::capacity_reserve_namespace::RESERVE_FILE_NAME
            || binding.protected_floor_bytes()
                != crate::durable_capacity::PROTECTED_FREE_SPACE_FLOOR
            || binding.owner_generation() >= volume_owner.owner_generation()
        {
            return Err(RecoveryReserveContextError::Binding(
                recovery_reserve_binding::ReserveBindingError::IdentityMismatch(
                    "source clean-handoff binding does not match the current owner generation"
                        .into(),
                ),
            ));
        }
        proof
            .validate_for_physical_use()
            .map_err(|error| RecoveryReserveContextError::Reserve(error))?;
        self.prove_clean_handoff_durable()
            .map_err(|error| RecoveryReserveContextError::Store(error))?;
        volume_owner.revalidate()?;
        let expected =
            super::capacity_reserve_namespace::ReserveNamespaceObservation::from_binding(
                binding.root_identity(),
                binding.file_identity(),
                binding.volume().clone(),
                binding.allocation_unit(),
            )
            .map_err(|reason| RecoveryReserveContextError::Reserve(reason.into()))?;
        let effect = proof.begin_effect()?;
        let reserve =
            super::capacity_reserve_namespace::reopen_owned_reserve(root, profile_guard, &expected)
                .map_err(|error| proof.namespace_error(error))?;
        let reserve = reserve
            .into_owned()
            .map_err(|error| proof.namespace_error(error))?;
        let mut reserve = OwnedCustody::new(reserve, proof);
        #[cfg(test)]
        self.notify_current_reserve_test_boundary(
            proof,
            CurrentReserveBoundary::CleanRebindOpened,
            reserve.file_for_custody_test(),
        );
        let result = (|| -> Result<(), RecoveryReserveContextError> {
            let physical = reserve
                .view(profile_guard)
                .revalidate_and_prove()
                .map_err(|error| RecoveryReserveContextError::Reserve(error.to_string()))?;
            if physical.volume() != volume_owner.volume()
                || !physical.non_sparse()
                || physical.physical_bytes() < crate::durable_capacity::PROTECTED_FREE_SPACE_FLOOR
            {
                return Err(RecoveryReserveContextError::Reserve(
                    "source clean-handoff reserve proof does not meet the protected floor".into(),
                ));
            }
            proof
                .validate_for_physical_use()
                .map_err(|error| RecoveryReserveContextError::Reserve(error))?;
            volume_owner.revalidate()?;
            let sql_effect = proof.begin_effect()?;
            recovery_reserve_binding::rebind_owner_generation(
                &self.connection,
                &binding,
                volume_owner.owner_generation(),
            )?;
            drop(sql_effect);
            #[cfg(test)]
            self.notify_current_reserve_test_boundary(
                proof,
                CurrentReserveBoundary::CleanRebound,
                reserve.file_for_custody_test(),
            );
            volume_owner.revalidate()?;
            proof
                .validate_for_physical_use()
                .map_err(RecoveryReserveContextError::Reserve)?;
            Ok(())
        })();
        if let Err(error) = result {
            return Err(error);
        }
        drop(effect);
        Ok(reserve.take())
    }

    /// Reopen a previously bound reserve after a crash-like owner restart.
    ///
    /// The durable binding is only an observation. The caller must provide the current profile,
    /// root, and per-volume lock capabilities; the fixed child is then reopened and revalidated
    /// from its live descriptor. A partial extent is accepted only when the active durable charge
    /// proves the minimum remaining protected extent.
    #[allow(dead_code)]
    pub(crate) fn reopen_recovery_reserve<'a>(
        &'a self,
        volume_owner: VolumeOwnershipCapability,
        root: &'a BoundDatabaseRoot,
        profile_guard: &'a WritableProfileGuard,
    ) -> Result<RecoveryReserveContext<'a>, RecoveryReserveContextError> {
        self.reopen_recovery_reserve_with_failure_proof(volume_owner, root, profile_guard, None)
    }

    fn reopen_recovery_reserve_with_failure_proof<'a>(
        &'a self,
        volume_owner: VolumeOwnershipCapability,
        root: &'a BoundDatabaseRoot,
        profile_guard: &'a WritableProfileGuard,
        failure_proof: Option<&ReserveBootstrapProof<'_, '_>>,
    ) -> Result<RecoveryReserveContext<'a>, RecoveryReserveContextError> {
        self.validate_namespace()?;
        root.validate_profile_guard(profile_guard)
            .map_err(OperationJournalStoreError::ProfileOwnership)?;
        let profile_identity = self.profile_guard.profile_root_identity().to_owned();
        if profile_guard.profile_root_identity() != profile_identity
            || volume_owner.owner_profile_identity() != profile_identity
        {
            return Err(RecoveryReserveContextError::ProfileIdentityMismatch);
        }
        volume_owner.revalidate()?;

        let binding = recovery_reserve_binding::read(&self.connection, volume_owner.volume())?
            .ok_or_else(|| {
                recovery_reserve_binding::ReserveBindingError::Missing(
                    volume_owner.volume().clone(),
                )
            })?;
        if binding.profile_identity() != profile_identity
            || binding.owner_generation() != volume_owner.owner_generation()
            || binding.volume() != volume_owner.volume()
            || binding.root_identity() != root.identity()
            || binding.namespace_version() != recovery_reserve_binding::NAMESPACE_VERSION
            || binding.namespace_name() != super::capacity_reserve_namespace::RESERVE_FILE_NAME
            || binding.protected_floor_bytes()
                != crate::durable_capacity::PROTECTED_FREE_SPACE_FLOOR
        {
            return Err(RecoveryReserveContextError::Binding(
                recovery_reserve_binding::ReserveBindingError::IdentityMismatch(
                    "durable reserve binding does not match the live owner/root".into(),
                ),
            ));
        }

        let active = recovery_reserve::active_charge_snapshot_for_volume(
            &self.connection,
            volume_owner.volume(),
        )?;
        let minimum_length = if let Some(snapshot) = active {
            let spent = snapshot
                .budget_bytes
                .checked_sub(snapshot.remaining_bytes)
                .ok_or_else(|| {
                    RecoveryReserveContextError::Reserve(
                        "durable reserve charge remaining bytes exceed its budget".into(),
                    )
                })?;
            let minimum = crate::durable_capacity::PROTECTED_FREE_SPACE_FLOOR
                .checked_sub(spent)
                .ok_or_else(|| {
                    RecoveryReserveContextError::Reserve(
                        "durable reserve spend exceeds the protected floor".into(),
                    )
                })?;
            if minimum < snapshot.margin_bytes {
                return Err(RecoveryReserveContextError::Reserve(
                    "durable reserve spend consumes its control margin".into(),
                ));
            }
            minimum
        } else {
            crate::durable_capacity::PROTECTED_FREE_SPACE_FLOOR
        };
        let expected =
            super::capacity_reserve_namespace::ReserveNamespaceObservation::from_binding(
                binding.root_identity(),
                binding.file_identity(),
                binding.volume().clone(),
                binding.allocation_unit(),
            )
            .map_err(|reason| RecoveryReserveContextError::Reserve(reason.into()))?;
        let _effect = match failure_proof {
            Some(proof) => proof.begin_effect()?,
            None => None,
        };
        let reserve = super::capacity_reserve_namespace::reopen_owned_reserve_partial(
            root,
            profile_guard,
            &expected,
            minimum_length,
        )
        .map_err(|error| match failure_proof {
            Some(proof) => proof.namespace_error(error),
            None => RecoveryReserveContextError::Reserve(error.to_string()),
        })?;
        let reserve = BoundCustody::new(reserve, failure_proof);
        #[cfg(test)]
        if let Some(proof) = failure_proof {
            self.notify_current_reserve_test_boundary(
                proof,
                CurrentReserveBoundary::RecoveryReopened,
                reserve.file_for_custody_test(),
            );
        }
        if reserve.profile_identity() != profile_identity
            || reserve.observation().volume() != volume_owner.volume()
            || reserve.observation().file_identity() != binding.file_identity()
            || reserve.observation().allocation_unit() != binding.allocation_unit()
        {
            return Err(RecoveryReserveContextError::Binding(
                recovery_reserve_binding::ReserveBindingError::IdentityMismatch(
                    "reopened reserve descriptor differs from the durable binding".into(),
                ),
            ));
        }
        if let Err(error) = volume_owner.revalidate() {
            return Err(error.into());
        }
        if let Some(proof) = failure_proof {
            if let Err(error) = proof.validate_for_physical_use() {
                return Err(RecoveryReserveContextError::Reserve(error));
            }
        }
        Ok(RecoveryReserveContext {
            store: self,
            volume_owner,
            reserve: reserve.take(),
            journal_identity: self.database_identity.clone(),
            profile_identity,
        })
    }

    #[cfg(test)]
    pub(super) fn retirement_whole_image_for_test(
        &self,
    ) -> Result<([u64; 5], Option<i64>), OperationJournalStoreError> {
        let tables = [
            "operation_journal",
            "operation_journal_full_progress",
            "operation_journal_staging_checkpoints",
            "operation_journal_chunk_staging",
            "operation_journal_recording_chunks",
        ];
        let mut counts = [0; 5];
        for (count, table) in counts.iter_mut().zip(tables) {
            *count =
                self.connection
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                        row.get::<_, u64>(0)
                    })?;
        }
        let sequence = self.connection.query_row(
            "SELECT MAX(sequence) FROM (
             SELECT sequence FROM operation_journal UNION ALL
             SELECT sequence FROM operation_journal_full_progress UNION ALL
             SELECT sequence FROM operation_journal_staging_checkpoints UNION ALL
             SELECT sequence FROM operation_journal_chunk_staging UNION ALL
             SELECT sequence FROM operation_journal_recording_chunks)",
            [],
            |row| row.get::<_, Option<i64>>(0),
        )?;
        Ok((counts, sequence))
    }

    fn open_inner(
        profile_guard: &WritableProfileGuard,
        #[cfg(test)] before_sqlite_open: Option<&dyn Fn(&Path)>,
    ) -> Result<Self, OperationJournalStoreError> {
        Self::open_inner_with_hooks(
            profile_guard,
            OpenHooks {
                #[cfg(test)]
                before_sqlite_open,
                migration_failure: None,
            },
        )
    }

    fn open_inner_with_hooks(
        profile_guard: &WritableProfileGuard,
        hooks: OpenHooks<'_>,
    ) -> Result<Self, OperationJournalStoreError> {
        let database_root = BoundDatabaseRoot::for_profile_child(
            profile_guard,
            Path::new(JOURNAL_DATABASE_ROOT_NAME),
        )?;
        database_root.validate_profile_guard(profile_guard)?;
        database_root.sync_namespace(profile_guard)?;

        let database_name = Path::new(JOURNAL_DATABASE_FILE_NAME);
        let database_path = database_root.path().join(database_name);
        let rollback_journal = inspect_preexisting_sidecars(&database_root, &database_path)?;

        let (mut file, created) =
            open_database_file(&database_root, database_name, &database_path)?;
        ensure_regular_file(&file, &database_path)?;
        let database_identity =
            stable_filesystem_identity_from_open_file(&file).ok_or_else(|| {
                OperationJournalStoreError::RequiresAudit {
                    path: database_path.clone(),
                    reason: String::from("stable database-file identity is unavailable"),
                }
            })?;

        let header_version = if created {
            None
        } else {
            Some(validate_sqlite_header(&mut file, &database_path)?)
        };
        if created {
            if file
                .metadata()
                .map_err(|source| io_error(&database_path, source))?
                .len()
                != 0
            {
                return Err(audit_error(
                    &database_path,
                    "a newly created journal file was populated before SQLite bootstrap",
                ));
            }
        }
        #[cfg(test)]
        if let Some(gate) = hooks.before_sqlite_open {
            gate(&database_path);
        }
        validate_pre_sqlite_sidecars(&database_root, &database_path, rollback_journal.as_ref())?;
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let connection = database_root.open_sqlite(database_name, flags)?;
        connection.set_limit(Limit::SQLITE_LIMIT_LENGTH, SQLITE_VALUE_LIMIT)?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        validate_connection_identity(
            &connection,
            &database_root,
            &database_path,
            &database_identity,
        )?;
        validate_database_file_identity(&database_root, &database_path, &database_identity)?;
        validate_pre_sqlite_sidecars(&database_root, &database_path, rollback_journal.as_ref())?;
        drop(file);

        if created {
            configure_connection(&connection)?;
            bootstrap_schema(&connection)?;
        } else {
            let connection_version = read_sqlite_user_version(&connection, &database_path)?;
            let header_version = header_version.expect("existing journal has a header version");
            if connection_version != header_version {
                return Err(audit_error(
                    &database_path,
                    format!(
                        "SQLite header user version {header_version} differs from PRAGMA user_version {connection_version}"
                    ),
                ));
            }
            validate_existing_schema(&connection, &database_path, header_version)?;
            if header_version == LEGACY_JOURNAL_SCHEMA_VERSION {
                if let Some(rollback_journal) = rollback_journal.as_ref() {
                    validate_rollback_journal_identity(
                        &database_root,
                        &database_path,
                        &rollback_journal.identity,
                    )?;
                }
                configure_connection(&connection)?;
                verify_connection_configuration(
                    &connection,
                    &database_path,
                    LEGACY_JOURNAL_SCHEMA_VERSION,
                )?;
                migrate_legacy_schema(
                    &connection,
                    &database_root,
                    &database_path,
                    &database_identity,
                    profile_guard,
                    rollback_journal.as_ref(),
                    hooks.migration_failure,
                )?;
            } else if header_version == PREVIOUS_JOURNAL_SCHEMA_VERSION {
                if let Some(rollback_journal) = rollback_journal.as_ref() {
                    validate_rollback_journal_identity(
                        &database_root,
                        &database_path,
                        &rollback_journal.identity,
                    )?;
                }
                configure_connection(&connection)?;
                verify_connection_configuration(
                    &connection,
                    &database_path,
                    PREVIOUS_JOURNAL_SCHEMA_VERSION,
                )?;
                migrate_previous_schema(
                    &connection,
                    &database_root,
                    &database_path,
                    &database_identity,
                    profile_guard,
                    rollback_journal.as_ref(),
                )?;
            } else if header_version == RECOVERY_RESERVE_SCHEMA_VERSION {
                if let Some(rollback_journal) = rollback_journal.as_ref() {
                    validate_rollback_journal_identity(
                        &database_root,
                        &database_path,
                        &rollback_journal.identity,
                    )?;
                }
                configure_connection(&connection)?;
                verify_connection_configuration(
                    &connection,
                    &database_path,
                    RECOVERY_RESERVE_SCHEMA_VERSION,
                )?;
                migrate_recovery_reserve_schema(
                    &connection,
                    &database_root,
                    &database_path,
                    &database_identity,
                    profile_guard,
                    rollback_journal.as_ref(),
                )?;
            } else if header_version == RECOVERY_RESERVE_BINDING_SCHEMA_VERSION {
                if let Some(rollback_journal) = rollback_journal.as_ref() {
                    validate_rollback_journal_identity(
                        &database_root,
                        &database_path,
                        &rollback_journal.identity,
                    )?;
                }
                configure_connection(&connection)?;
                verify_connection_configuration(
                    &connection,
                    &database_path,
                    RECOVERY_RESERVE_BINDING_SCHEMA_VERSION,
                )?;
                migrate_ordinary_claim_schema(
                    &connection,
                    &database_root,
                    &database_path,
                    &database_identity,
                    profile_guard,
                    rollback_journal.as_ref(),
                )?;
            } else if header_version == ORDINARY_CLAIM_SCHEMA_VERSION {
                if let Some(rollback_journal) = rollback_journal.as_ref() {
                    validate_rollback_journal_identity(
                        &database_root,
                        &database_path,
                        &rollback_journal.identity,
                    )?;
                }
                configure_connection(&connection)?;
                verify_connection_configuration(
                    &connection,
                    &database_path,
                    ORDINARY_CLAIM_SCHEMA_VERSION,
                )?;
                migrate_ordinary_claim_release_schema(
                    &connection,
                    &database_root,
                    &database_path,
                    &database_identity,
                    profile_guard,
                    rollback_journal.as_ref(),
                )?;
            } else if header_version == ORDINARY_CLAIM_RELEASE_SCHEMA_VERSION {
                if let Some(rollback_journal) = rollback_journal.as_ref() {
                    validate_rollback_journal_identity(
                        &database_root,
                        &database_path,
                        &rollback_journal.identity,
                    )?;
                }
                configure_connection(&connection)?;
                verify_connection_configuration(
                    &connection,
                    &database_path,
                    ORDINARY_CLAIM_RELEASE_SCHEMA_VERSION,
                )?;
                migrate_chunk_claim_schema(
                    &connection,
                    &database_root,
                    &database_path,
                    &database_identity,
                    profile_guard,
                    rollback_journal.as_ref(),
                )?;
            } else if header_version == CHUNK_CLAIM_SCHEMA_VERSION {
                if let Some(rollback_journal) = rollback_journal.as_ref() {
                    validate_rollback_journal_identity(
                        &database_root,
                        &database_path,
                        &rollback_journal.identity,
                    )?;
                }
                configure_connection(&connection)?;
                verify_connection_configuration(
                    &connection,
                    &database_path,
                    CHUNK_CLAIM_SCHEMA_VERSION,
                )?;
                migrate_staging_checkpoint_schema(
                    &connection,
                    &database_root,
                    &database_path,
                    &database_identity,
                    profile_guard,
                    rollback_journal.as_ref(),
                )?;
            } else if header_version == STAGING_CHECKPOINT_SCHEMA_VERSION {
                if let Some(rollback_journal) = rollback_journal.as_ref() {
                    validate_rollback_journal_identity(
                        &database_root,
                        &database_path,
                        &rollback_journal.identity,
                    )?;
                }
                configure_connection(&connection)?;
                verify_connection_configuration(
                    &connection,
                    &database_path,
                    STAGING_CHECKPOINT_SCHEMA_VERSION,
                )?;
                migrate_chunk_staging_schema(
                    &connection,
                    &database_root,
                    &database_path,
                    &database_identity,
                    profile_guard,
                    rollback_journal.as_ref(),
                )?;
            } else if header_version == CHUNK_STAGING_SCHEMA_VERSION {
                configure_connection(&connection)?;
                migrate_recording_chunks_schema(
                    &connection,
                    &database_root,
                    &database_path,
                    &database_identity,
                    profile_guard,
                    rollback_journal.as_ref(),
                )?;
            } else if header_version == RECORDING_CHUNKS_SCHEMA_VERSION {
                configure_connection(&connection)?;
                migrate_database_claim_schema(
                    &connection,
                    &database_root,
                    &database_path,
                    &database_identity,
                    profile_guard,
                    rollback_journal.as_ref(),
                )?;
            } else if header_version == DATABASE_CLAIMS_SCHEMA_VERSION {
                configure_connection(&connection)?;
                migrate_database_attempt_schema(
                    &connection,
                    &database_root,
                    &database_path,
                    &database_identity,
                    profile_guard,
                    rollback_journal.as_ref(),
                )?;
            } else if header_version == DATABASE_ATTEMPTS_SCHEMA_VERSION {
                configure_connection(&connection)?;
                migrate_publication_binding_schema(
                    &connection,
                    &database_root,
                    &database_path,
                    &database_identity,
                    profile_guard,
                    rollback_journal.as_ref(),
                )?;
            } else {
                configure_connection(&connection)?;
            }
        }
        if let Some(rollback_journal) = rollback_journal.as_ref() {
            validate_rollback_journal_identity(
                &database_root,
                &database_path,
                &rollback_journal.identity,
            )?;
        }
        validate_connection_identity(
            &connection,
            &database_root,
            &database_path,
            &database_identity,
        )?;
        verify_connection_configuration(&connection, &database_path, JOURNAL_SCHEMA_VERSION)?;
        database_root.sync_namespace(profile_guard)?;
        database_root.validate_profile_guard(profile_guard)?;
        drop(rollback_journal);

        // Clone only after every open, header, schema, pragma, and identity check has succeeded.
        // The clone is the final retained owner moved into the store.
        let retained_profile_guard = profile_guard.try_clone()?;
        Ok(Self {
            connection,
            recording_prestart_scope: std::cell::Cell::new(false),
            recording_prestart_resource_refused: std::cell::Cell::new(false),
            database_path,
            database_identity,
            database_root,
            profile_guard: retained_profile_guard,
            #[cfg(test)]
            recording_readback_corruption: std::sync::Mutex::new(None),
            #[cfg(test)]
            current_reserve_test_hook: std::sync::Mutex::new(None),
            #[cfg(all(test, unix))]
            current_reopen_fixture: std::cell::Cell::new(false),
            #[cfg(test)]
            insert_test_hook: std::sync::Mutex::new(None),
            #[cfg(test)]
            release_test_hook: std::sync::Mutex::new(None),
        })
    }

    /// Return one bounded operation record by id.
    pub fn get(
        &self,
        operation_id: OperationId,
    ) -> Result<Option<StoredOperationRecord>, OperationJournalStoreError> {
        self.validate_namespace()?;
        let Some(row) = self.read_row(operation_id)? else {
            self.validate_namespace()?;
            return Ok(None);
        };
        self.validate_namespace()?;
        Ok(Some(row.record))
    }

    /// Persist one validated V5 regular-file copy intent.
    ///
    /// This method records the typed command and its participant observations after an owning
    /// coordinator has completed any queue and capacity gates. It proves only that the V5 intent
    /// reached this journal; it does not admit filesystem execution or acquire a live capability.
    /// As a compatibility adapter, an `Err` may be returned after COMMIT and therefore does not
    /// prove that the operation row is absent. Claim or admission code should use
    /// [`Self::persist_copy_intent_with_outcome`] when it needs the write-stage observation.
    pub fn persist_copy_intent(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: CopyIntentArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Result<JournalInsertReceipt, OperationJournalStoreError> {
        self.persist_copy_intent_with_outcome(
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
            timestamp_unix_ms,
        )
        .into_result()
        .map_err(JournalInsertFailure::into_error)
    }

    /// Persist one V5 copy intent and expose the exact write-stage outcome.
    ///
    /// A `Committed` outcome is returned only after the real SQLite commit, full namespace
    /// validation, and exact post-commit readback all succeed. A `Failed` outcome retains the
    /// original store error and never exposes a tentative receipt. This method records a durable
    /// intent only; it grants no filesystem admission, capacity authority, or execution token.
    pub fn persist_copy_intent_with_outcome(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: CopyIntentArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> JournalInsertOutcome {
        if intent.actor != OperationActor::User || intent.kind != OperationKind::FileMutation {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from(
                        "schema-v5 copy intent requires a user file-mutation operation",
                    ),
                },
                None,
                0,
                None,
            );
        }
        if universal_intent.command_kind() != super::model::UniversalCommandKind::Copy {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from("schema-v5 copy intent requires the Copy command"),
                },
                None,
                0,
                None,
            );
        }
        if timestamp_unix_ms < 0 {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from("schema-v5 timestamps must be non-negative"),
                },
                None,
                0,
                None,
            );
        }
        if let Err(error) =
            command_args.validate_against_participants(universal_intent.participants())
        {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: error.to_string(),
                },
                None,
                0,
                None,
            );
        }
        let record = StoredOperationRecord::new_v5_copy(
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
            timestamp_unix_ms,
        );
        match classify_record(&record) {
            RecordConsistency::Consistent => {}
            RecordConsistency::LegacyMissingPublicationEvidence => {
                return failed_insert(
                    operation_id,
                    JournalInsertWriteStage::NoMutationAttempted,
                    OperationJournalStoreError::InconsistentRecord {
                        reason: String::from(
                            "schema-v5 copy intent has legacy publication evidence",
                        ),
                    },
                    None,
                    0,
                    None,
                );
            }
            RecordConsistency::Invalid(reason) => {
                return failed_insert(
                    operation_id,
                    JournalInsertWriteStage::NoMutationAttempted,
                    OperationJournalStoreError::InconsistentRecord { reason },
                    None,
                    0,
                    None,
                );
            }
        }
        let encoded = match encode(&record) {
            Ok(encoded) => encoded,
            Err(error) => {
                return failed_insert(
                    operation_id,
                    JournalInsertWriteStage::NoMutationAttempted,
                    error.into(),
                    None,
                    0,
                    None,
                );
            }
        };
        self.persist_record_with_encoded_outcome(&record, &encoded)
    }

    /// Persist one V6 typed coordinator command and return its compatibility receipt.
    ///
    /// The command arguments and participant observations are stored as bounded, authority-free
    /// data. An `Err` may be returned after COMMIT, so callers that need to distinguish an
    /// unacknowledged commit must use [`Self::persist_command_intent_with_outcome`].
    pub fn persist_command_intent(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: UniversalCommandArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Result<JournalInsertReceipt, OperationJournalStoreError> {
        self.persist_command_intent_with_outcome(
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
            timestamp_unix_ms,
        )
        .into_result()
        .map_err(JournalInsertFailure::into_error)
    }

    /// Persist one V6 typed coordinator command and expose the exact write-stage outcome.
    ///
    /// This is a storage acknowledgement only. It does not admit execution, reserve capacity,
    /// retain a filesystem root, or grant a live source capability. The returned receipt is
    /// exposed only after commit, full post-commit namespace validation, and exact readback.
    pub fn persist_command_intent_with_outcome(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: UniversalCommandArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> JournalInsertOutcome {
        if intent.actor != OperationActor::User || intent.kind != OperationKind::FileMutation {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from(
                        "schema-v6 command intent requires a user file-mutation operation",
                    ),
                },
                None,
                0,
                None,
            );
        }
        if universal_intent.command_kind() != command_args.command_kind() {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from(
                        "schema-v6 command kind does not match tagged command arguments",
                    ),
                },
                None,
                0,
                None,
            );
        }
        if timestamp_unix_ms < 0 {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from("schema-v6 timestamps must be non-negative"),
                },
                None,
                0,
                None,
            );
        }
        if let Err(error) =
            command_args.validate_against_participants(universal_intent.participants())
        {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: error.to_string(),
                },
                None,
                0,
                None,
            );
        }
        let record = StoredOperationRecord::new_v6_command(
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
            timestamp_unix_ms,
        );
        match classify_record(&record) {
            RecordConsistency::Consistent => {}
            RecordConsistency::LegacyMissingPublicationEvidence => {
                return failed_insert(
                    operation_id,
                    JournalInsertWriteStage::NoMutationAttempted,
                    OperationJournalStoreError::InconsistentRecord {
                        reason: String::from(
                            "schema-v6 command intent has legacy publication evidence",
                        ),
                    },
                    None,
                    0,
                    None,
                );
            }
            RecordConsistency::Invalid(reason) => {
                return failed_insert(
                    operation_id,
                    JournalInsertWriteStage::NoMutationAttempted,
                    OperationJournalStoreError::InconsistentRecord { reason },
                    None,
                    0,
                    None,
                );
            }
        }
        let encoded = match encode(&record) {
            Ok(encoded) => encoded,
            Err(error) => {
                return failed_insert(
                    operation_id,
                    JournalInsertWriteStage::NoMutationAttempted,
                    error.into(),
                    None,
                    0,
                    None,
                );
            }
        };
        self.persist_record_with_encoded_outcome(&record, &encoded)
    }

    /// Persist an existing tagged command in the V7 envelope and return its compatibility receipt.
    ///
    /// This explicit V7 entry point preserves V6 command observations while allowing a later
    /// caller to select the V7 envelope. It does not change the older V6 adapter or grant any
    /// execution authority.
    pub fn persist_command_intent_v7(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: UniversalCommandArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Result<JournalInsertReceipt, OperationJournalStoreError> {
        self.persist_command_intent_v7_with_outcome(
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
            timestamp_unix_ms,
        )
        .into_result()
        .map_err(JournalInsertFailure::into_error)
    }

    /// Persist an existing tagged command in the V7 envelope with its exact write-stage outcome.
    pub fn persist_command_intent_v7_with_outcome(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: UniversalCommandArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> JournalInsertOutcome {
        if intent.actor != OperationActor::User || intent.kind != OperationKind::FileMutation {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from(
                        "schema-v7 command intent requires a user file-mutation operation",
                    ),
                },
                None,
                0,
                None,
            );
        }
        if universal_intent.command_kind() != command_args.command_kind() {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from(
                        "schema-v7 command kind does not match tagged command arguments",
                    ),
                },
                None,
                0,
                None,
            );
        }
        if timestamp_unix_ms < 0 {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from("schema-v7 timestamps must be non-negative"),
                },
                None,
                0,
                None,
            );
        }
        if let Err(error) =
            command_args.validate_against_participants(universal_intent.participants())
        {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: error.to_string(),
                },
                None,
                0,
                None,
            );
        }
        let record = StoredOperationRecord::new_v7_command(
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
            timestamp_unix_ms,
        );
        self.insert_intent_with_outcome(&record)
    }

    /// Persist one V8 command envelope and return its compatibility receipt.
    ///
    /// V8 carries the bounded namespace mutation commands while preserving the earlier Copy,
    /// Import, DuplicateSame, and WaveformEdit observations. This remains a persistence
    /// acknowledgement only; it does not authorize filesystem work or reserve capacity.
    pub fn persist_command_intent_v8(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: UniversalCommandArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Result<JournalInsertReceipt, OperationJournalStoreError> {
        self.persist_command_intent_v8_with_outcome(
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
            timestamp_unix_ms,
        )
        .into_result()
        .map_err(JournalInsertFailure::into_error)
    }

    /// Persist one V8 command envelope with its exact write-stage outcome.
    pub fn persist_command_intent_v8_with_outcome(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: UniversalCommandArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> JournalInsertOutcome {
        if intent.actor != OperationActor::User || intent.kind != OperationKind::FileMutation {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from(
                        "schema-v8 command intent requires a user file-mutation operation",
                    ),
                },
                None,
                0,
                None,
            );
        }
        if universal_intent.command_kind() != command_args.command_kind() {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from(
                        "schema-v8 command kind does not match tagged command arguments",
                    ),
                },
                None,
                0,
                None,
            );
        }
        if timestamp_unix_ms < 0 {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from("schema-v8 timestamps must be non-negative"),
                },
                None,
                0,
                None,
            );
        }
        if let Err(error) =
            command_args.validate_against_participants(universal_intent.participants())
        {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: error.to_string(),
                },
                None,
                0,
                None,
            );
        }
        let record = StoredOperationRecord::new_v8_command(
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
            timestamp_unix_ms,
        );
        self.insert_intent_with_outcome(&record)
    }

    /// Persist one V9 whole-file render command and return its compatibility receipt.
    ///
    /// V9 preserves the V6, V7, and V8 tagged observations and adds Normalize and DuplicateDouble
    /// render observations. The record is durable intent data only; this call performs no
    /// filesystem work, capacity reservation, source lease acquisition, or publication.
    pub fn persist_command_intent_v9(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: UniversalCommandArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Result<JournalInsertReceipt, OperationJournalStoreError> {
        self.persist_command_intent_v9_with_outcome(
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
            timestamp_unix_ms,
        )
        .into_result()
        .map_err(JournalInsertFailure::into_error)
    }

    /// Persist one V9 whole-file render command with its exact write-stage outcome.
    pub fn persist_command_intent_v9_with_outcome(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: UniversalCommandArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> JournalInsertOutcome {
        if intent.actor != OperationActor::User || intent.kind != OperationKind::FileMutation {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from(
                        "schema-v9 render intent requires a user file-mutation operation",
                    ),
                },
                None,
                0,
                None,
            );
        }
        if universal_intent.command_kind() != command_args.command_kind() {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from(
                        "schema-v9 command kind does not match tagged command arguments",
                    ),
                },
                None,
                0,
                None,
            );
        }
        if timestamp_unix_ms < 0 {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from("schema-v9 timestamps must be non-negative"),
                },
                None,
                0,
                None,
            );
        }
        if let Err(error) =
            command_args.validate_against_participants(universal_intent.participants())
        {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: error.to_string(),
                },
                None,
                0,
                None,
            );
        }
        let record = StoredOperationRecord::new_v9_command(
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
            timestamp_unix_ms,
        );
        self.insert_intent_with_outcome(&record)
    }

    /// Persist one V10 configured-removal command and return its compatibility receipt.
    ///
    /// V10 carries only the initial configured-trash and source-local retained-folder request.
    /// It performs no filesystem work, recovery staging, metadata capture, or permanent deletion.
    pub fn persist_command_intent_v10(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: UniversalCommandArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Result<JournalInsertReceipt, OperationJournalStoreError> {
        self.persist_command_intent_v10_with_outcome(
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
            timestamp_unix_ms,
        )
        .into_result()
        .map_err(JournalInsertFailure::into_error)
    }

    /// Persist one V10 configured-removal command with its exact write-stage outcome.
    pub fn persist_command_intent_v10_with_outcome(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: UniversalCommandArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> JournalInsertOutcome {
        if intent.actor != OperationActor::User || intent.kind != OperationKind::FileMutation {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from(
                        "schema-v10 removal intent requires a user file-mutation operation",
                    ),
                },
                None,
                0,
                None,
            );
        }
        if universal_intent.command_kind() != command_args.command_kind() {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from(
                        "schema-v10 removal command kind does not match tagged command arguments",
                    ),
                },
                None,
                0,
                None,
            );
        }
        if timestamp_unix_ms < 0 {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from("schema-v10 timestamps must be non-negative"),
                },
                None,
                0,
                None,
            );
        }
        if let Err(error) =
            command_args.validate_against_participants(universal_intent.participants())
        {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: error.to_string(),
                },
                None,
                0,
                None,
            );
        }
        let record = StoredOperationRecord::new_v10_command(
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
            timestamp_unix_ms,
        );
        self.insert_intent_with_outcome(&record)
    }

    /// Persist one V11 retained-folder restore or purge command and return its compatibility receipt.
    ///
    /// V11 records only the bounded retained-entry observation and explicit purge decision. It
    /// performs no legacy-journal import, metadata capture, restore merge, purge, or filesystem
    /// work, and the stored observations grant no execution authority.
    pub fn persist_command_intent_v11(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: UniversalCommandArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Result<JournalInsertReceipt, OperationJournalStoreError> {
        self.persist_command_intent_v11_with_outcome(
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
            timestamp_unix_ms,
        )
        .into_result()
        .map_err(JournalInsertFailure::into_error)
    }

    /// Persist one V11 retained-folder command with its exact write-stage outcome.
    pub fn persist_command_intent_v11_with_outcome(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: UniversalCommandArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> JournalInsertOutcome {
        if intent.actor != OperationActor::User || intent.kind != OperationKind::FileMutation {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from(
                        "schema-v11 retained resolution intent requires a user file-mutation operation",
                    ),
                },
                None,
                0,
                None,
            );
        }
        if universal_intent.command_kind() != command_args.command_kind() {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from(
                        "schema-v11 retained resolution command kind does not match tagged command arguments",
                    ),
                },
                None,
                0,
                None,
            );
        }
        if !matches!(command_args, UniversalCommandArgs::RetainedResolution(_)) {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from(
                        "schema-v11 retained resolution intent requires RetainedResolution arguments",
                    ),
                },
                None,
                0,
                None,
            );
        }
        if timestamp_unix_ms < 0 {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from("schema-v11 timestamps must be non-negative"),
                },
                None,
                0,
                None,
            );
        }
        if let Err(error) =
            command_args.validate_against_participants(universal_intent.participants())
        {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: error.to_string(),
                },
                None,
                0,
                None,
            );
        }
        let record = StoredOperationRecord::new_v11_command(
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
            timestamp_unix_ms,
        );
        self.insert_intent_with_outcome(&record)
    }

    /// Persist one typed V11 retained-folder restore or purge command.
    pub fn persist_retained_resolution_intent(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: RetainedResolutionCommandArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Result<JournalInsertReceipt, OperationJournalStoreError> {
        self.persist_retained_resolution_intent_with_outcome(
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
            timestamp_unix_ms,
        )
        .into_result()
        .map_err(JournalInsertFailure::into_error)
    }

    /// Persist one typed V11 retained-folder command with its exact write-stage outcome.
    pub fn persist_retained_resolution_intent_with_outcome(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: RetainedResolutionCommandArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> JournalInsertOutcome {
        self.persist_command_intent_v11_with_outcome(
            operation_id,
            intent,
            universal_intent,
            UniversalCommandArgs::RetainedResolution(command_args),
            capacity_plan,
            timestamp_unix_ms,
        )
    }

    /// Persist one typed V11 retained-folder restore observation.
    pub fn persist_retained_restore_intent(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: RetainedRestoreIntentArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Result<JournalInsertReceipt, OperationJournalStoreError> {
        self.persist_retained_resolution_intent(
            operation_id,
            intent,
            universal_intent,
            RetainedResolutionCommandArgs::Restore(command_args),
            capacity_plan,
            timestamp_unix_ms,
        )
    }

    /// Persist one typed V11 retained-folder purge observation.
    pub fn persist_retained_purge_intent(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: RetainedPurgeIntentArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Result<JournalInsertReceipt, OperationJournalStoreError> {
        self.persist_retained_resolution_intent(
            operation_id,
            intent,
            universal_intent,
            RetainedResolutionCommandArgs::Purge(command_args),
            capacity_plan,
            timestamp_unix_ms,
        )
    }

    /// Persist one V12 selection-only Normalize command and return its compatibility receipt.
    ///
    /// V12 records the source revision, WAV layout, exact selection, resolved edge-fade frame
    /// count, and full-file Float32 output. It performs no audio I/O, peak calculation, capacity
    /// reservation, source lease acquisition, filesystem work, or publication.
    pub fn persist_command_intent_v12(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: UniversalCommandArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Result<JournalInsertReceipt, OperationJournalStoreError> {
        self.persist_command_intent_v12_with_outcome(
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
            timestamp_unix_ms,
        )
        .into_result()
        .map_err(JournalInsertFailure::into_error)
    }

    /// Persist one V12 selection Normalize command with its exact write-stage outcome.
    pub fn persist_command_intent_v12_with_outcome(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: UniversalCommandArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> JournalInsertOutcome {
        if intent.actor != OperationActor::User || intent.kind != OperationKind::FileMutation {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from(
                        "schema-v12 selection Normalize intent requires a user file-mutation operation",
                    ),
                },
                None,
                0,
                None,
            );
        }
        if universal_intent.command_kind() != command_args.command_kind() {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from(
                        "schema-v12 selection Normalize command kind does not match tagged command arguments",
                    ),
                },
                None,
                0,
                None,
            );
        }
        if !matches!(command_args, UniversalCommandArgs::SelectionNormalize(_)) {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from(
                        "schema-v12 selection Normalize intent requires SelectionNormalize arguments",
                    ),
                },
                None,
                0,
                None,
            );
        }
        if timestamp_unix_ms < 0 {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from("schema-v12 timestamps must be non-negative"),
                },
                None,
                0,
                None,
            );
        }
        if let Err(error) =
            command_args.validate_against_participants(universal_intent.participants())
        {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: error.to_string(),
                },
                None,
                0,
                None,
            );
        }
        let record = StoredOperationRecord::new_v12_command(
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
            timestamp_unix_ms,
        );
        self.insert_intent_with_outcome(&record)
    }

    /// Persist one typed V12 selection Normalize observation.
    pub fn persist_selection_normalize_intent(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: SelectionNormalizeIntentArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Result<JournalInsertReceipt, OperationJournalStoreError> {
        self.persist_selection_normalize_intent_with_outcome(
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
            timestamp_unix_ms,
        )
        .into_result()
        .map_err(JournalInsertFailure::into_error)
    }

    /// Persist one typed V12 selection Normalize observation with its exact write-stage outcome.
    pub fn persist_selection_normalize_intent_with_outcome(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: SelectionNormalizeIntentArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> JournalInsertOutcome {
        self.persist_command_intent_v12_with_outcome(
            operation_id,
            intent,
            universal_intent,
            UniversalCommandArgs::SelectionNormalize(command_args),
            capacity_plan,
            timestamp_unix_ms,
        )
    }

    /// Persist one V13 source-owned selection-output command and return its compatibility receipt.
    pub fn persist_command_intent_v13(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: UniversalCommandArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Result<JournalInsertReceipt, OperationJournalStoreError> {
        self.persist_command_intent_v13_with_outcome(
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
            timestamp_unix_ms,
        )
        .into_result()
        .map_err(JournalInsertFailure::into_error)
    }

    /// Persist one V13 source-owned selection-output command with its exact write-stage outcome.
    pub fn persist_command_intent_v13_with_outcome(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: UniversalCommandArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> JournalInsertOutcome {
        if intent.actor != OperationActor::User || intent.kind != OperationKind::FileMutation {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from(
                        "schema-v13 selection output intent requires a user file-mutation operation",
                    ),
                },
                None,
                0,
                None,
            );
        }
        if universal_intent.command_kind() != command_args.command_kind() {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from(
                        "schema-v13 selection output command kind does not match tagged command arguments",
                    ),
                },
                None,
                0,
                None,
            );
        }
        if !matches!(command_args, UniversalCommandArgs::SelectionOutput(_)) {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from(
                        "schema-v13 selection output intent requires SelectionOutput arguments",
                    ),
                },
                None,
                0,
                None,
            );
        }
        if timestamp_unix_ms < 0 {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from("schema-v13 timestamps must be non-negative"),
                },
                None,
                0,
                None,
            );
        }
        if let Err(error) =
            command_args.validate_against_participants(universal_intent.participants())
        {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: error.to_string(),
                },
                None,
                0,
                None,
            );
        }
        let record = StoredOperationRecord::new_v13_command(
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
            timestamp_unix_ms,
        );
        self.insert_intent_with_outcome(&record)
    }

    /// Persist one V14 tagged command, retaining the old V6-V13 families and the new history
    /// family under their respective operation kinds.
    pub fn persist_command_intent_v14(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: UniversalCommandArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Result<JournalInsertReceipt, OperationJournalStoreError> {
        self.persist_command_intent_v14_with_outcome(
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
            timestamp_unix_ms,
        )
        .into_result()
        .map_err(JournalInsertFailure::into_error)
    }

    /// Persist one V14 tagged command and expose its exact write-stage outcome.
    pub fn persist_command_intent_v14_with_outcome(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: UniversalCommandArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> JournalInsertOutcome {
        let expected_kind = if matches!(&command_args, UniversalCommandArgs::History(_)) {
            OperationKind::FileHistory
        } else {
            OperationKind::FileMutation
        };
        if intent.actor != OperationActor::User || intent.kind != expected_kind {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from(
                        "schema-v14 command intent operation kind does not match its command family",
                    ),
                },
                None,
                0,
                None,
            );
        }
        if universal_intent.command_kind() != command_args.command_kind() {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from(
                        "schema-v14 command kind does not match tagged command arguments",
                    ),
                },
                None,
                0,
                None,
            );
        }
        if timestamp_unix_ms < 0 {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from("schema-v14 timestamps must be non-negative"),
                },
                None,
                0,
                None,
            );
        }
        if let Err(error) =
            command_args.validate_against_participants(universal_intent.participants())
        {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: error.to_string(),
                },
                None,
                0,
                None,
            );
        }
        let record = StoredOperationRecord::new_v14_command(
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
            timestamp_unix_ms,
        );
        match classify_record(&record) {
            RecordConsistency::Consistent => {}
            RecordConsistency::LegacyMissingPublicationEvidence => {
                return failed_insert(
                    operation_id,
                    JournalInsertWriteStage::NoMutationAttempted,
                    OperationJournalStoreError::InconsistentRecord {
                        reason: String::from(
                            "schema-v14 command intent has legacy publication evidence",
                        ),
                    },
                    None,
                    0,
                    None,
                );
            }
            RecordConsistency::Invalid(reason) => {
                return failed_insert(
                    operation_id,
                    JournalInsertWriteStage::NoMutationAttempted,
                    OperationJournalStoreError::InconsistentRecord { reason },
                    None,
                    0,
                    None,
                );
            }
        }
        let encoded = match encode(&record) {
            Ok(encoded) => encoded,
            Err(error) => {
                return failed_insert(
                    operation_id,
                    JournalInsertWriteStage::NoMutationAttempted,
                    error.into(),
                    None,
                    0,
                    None,
                );
            }
        };
        self.persist_record_with_encoded_outcome(&record, &encoded)
    }

    /// Persist one typed V14 transaction-history Undo or Redo observation.
    pub fn persist_history_intent(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: HistoryUndoRedoIntentArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Result<JournalInsertReceipt, OperationJournalStoreError> {
        self.persist_history_intent_with_outcome(
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
            timestamp_unix_ms,
        )
        .into_result()
        .map_err(JournalInsertFailure::into_error)
    }

    /// Persist one typed V14 transaction-history observation with its exact write-stage outcome.
    pub fn persist_history_intent_with_outcome(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: HistoryUndoRedoIntentArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> JournalInsertOutcome {
        self.persist_command_intent_v14_with_outcome(
            operation_id,
            intent,
            universal_intent,
            UniversalCommandArgs::History(command_args),
            capacity_plan,
            timestamp_unix_ms,
        )
    }

    /// Persist one V15 external filesystem reconciliation command.
    pub fn persist_command_intent_v15(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: UniversalCommandArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Result<JournalInsertReceipt, OperationJournalStoreError> {
        self.persist_command_intent_v15_with_outcome(
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
            timestamp_unix_ms,
        )
        .into_result()
        .map_err(JournalInsertFailure::into_error)
    }

    /// Persist one V15 external filesystem reconciliation command with its exact write-stage
    /// outcome.
    pub fn persist_command_intent_v15_with_outcome(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: UniversalCommandArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> JournalInsertOutcome {
        if intent.actor != OperationActor::ExternalFs
            || intent.kind != OperationKind::ExternalReconciliation
        {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from(
                        "schema-v15 command intent requires an external-fs external-reconciliation operation",
                    ),
                },
                None,
                0,
                None,
            );
        }
        if !matches!(
            command_args,
            UniversalCommandArgs::ExternalReconciliation(_)
        ) {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from(
                        "schema-v15 command intent requires ExternalReconciliation arguments",
                    ),
                },
                None,
                0,
                None,
            );
        }
        if universal_intent.command_kind() != command_args.command_kind() {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from(
                        "schema-v15 command kind does not match tagged command arguments",
                    ),
                },
                None,
                0,
                None,
            );
        }
        if timestamp_unix_ms < 0 {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from("schema-v15 timestamps must be non-negative"),
                },
                None,
                0,
                None,
            );
        }
        if let Err(error) =
            command_args.validate_against_participants(universal_intent.participants())
        {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: error.to_string(),
                },
                None,
                0,
                None,
            );
        }
        let record = StoredOperationRecord::new_v15_command(
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
            timestamp_unix_ms,
        );
        self.insert_intent_with_outcome(&record)
    }

    /// Persist one typed V15 external filesystem reconciliation observation.
    pub fn persist_external_reconciliation_intent(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: ExternalReconciliationIntentArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Result<JournalInsertReceipt, OperationJournalStoreError> {
        self.persist_external_reconciliation_intent_with_outcome(
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
            timestamp_unix_ms,
        )
        .into_result()
        .map_err(JournalInsertFailure::into_error)
    }

    /// Persist one typed V15 external filesystem reconciliation observation with its exact
    /// write-stage outcome.
    pub fn persist_external_reconciliation_intent_with_outcome(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: ExternalReconciliationIntentArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> JournalInsertOutcome {
        self.persist_command_intent_v15_with_outcome(
            operation_id,
            intent,
            universal_intent,
            UniversalCommandArgs::ExternalReconciliation(command_args),
            capacity_plan,
            timestamp_unix_ms,
        )
    }

    /// Persist one typed V13 raw WAV extraction command.
    pub fn persist_selection_extract_intent(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: SelectionExtractIntentArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Result<JournalInsertReceipt, OperationJournalStoreError> {
        self.persist_selection_extract_intent_with_outcome(
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
            timestamp_unix_ms,
        )
        .into_result()
        .map_err(JournalInsertFailure::into_error)
    }

    /// Persist one typed V13 raw WAV extraction command with its exact write-stage outcome.
    pub fn persist_selection_extract_intent_with_outcome(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: SelectionExtractIntentArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> JournalInsertOutcome {
        self.persist_command_intent_v13_with_outcome(
            operation_id,
            intent,
            universal_intent,
            UniversalCommandArgs::SelectionOutput(SelectionOutputCommandArgs::Extract(
                command_args,
            )),
            capacity_plan,
            timestamp_unix_ms,
        )
    }

    /// Persist one typed V13 ordered extraction-then-trim command.
    pub fn persist_extract_and_trim_intent(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: ExtractAndTrimIntentArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Result<JournalInsertReceipt, OperationJournalStoreError> {
        self.persist_extract_and_trim_intent_with_outcome(
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
            timestamp_unix_ms,
        )
        .into_result()
        .map_err(JournalInsertFailure::into_error)
    }

    /// Persist one typed V13 ordered extraction-then-trim command with its exact write-stage
    /// outcome.
    pub fn persist_extract_and_trim_intent_with_outcome(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: ExtractAndTrimIntentArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> JournalInsertOutcome {
        self.persist_command_intent_v13_with_outcome(
            operation_id,
            intent,
            universal_intent,
            UniversalCommandArgs::SelectionOutput(SelectionOutputCommandArgs::ExtractAndTrim(
                command_args,
            )),
            capacity_plan,
            timestamp_unix_ms,
        )
    }

    /// Persist one typed V10 removal command.
    pub fn persist_removal_command_intent(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: RemovalCommandArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Result<JournalInsertReceipt, OperationJournalStoreError> {
        self.persist_command_intent_v10(
            operation_id,
            intent,
            universal_intent,
            UniversalCommandArgs::Removal(command_args),
            capacity_plan,
            timestamp_unix_ms,
        )
    }

    /// Persist one typed configured-trash request.
    pub fn persist_trash_intent(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: TrashIntentArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Result<JournalInsertReceipt, OperationJournalStoreError> {
        self.persist_removal_command_intent(
            operation_id,
            intent,
            universal_intent,
            RemovalCommandArgs::Trash(command_args),
            capacity_plan,
            timestamp_unix_ms,
        )
    }

    /// Persist one typed source-local retained-folder request.
    pub fn persist_stage_retained_folder_intent(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: StageRetainedFolderIntentArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Result<JournalInsertReceipt, OperationJournalStoreError> {
        self.persist_removal_command_intent(
            operation_id,
            intent,
            universal_intent,
            RemovalCommandArgs::StageRetainedFolder(command_args),
            capacity_plan,
            timestamp_unix_ms,
        )
    }

    /// Persist one typed V9 whole-file render command and return its compatibility receipt.
    pub fn persist_render_command_intent(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: RenderCommandArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Result<JournalInsertReceipt, OperationJournalStoreError> {
        self.persist_render_command_intent_with_outcome(
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
            timestamp_unix_ms,
        )
        .into_result()
        .map_err(JournalInsertFailure::into_error)
    }

    /// Persist one typed V9 whole-file render command with its exact write-stage outcome.
    pub fn persist_render_command_intent_with_outcome(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: RenderCommandArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> JournalInsertOutcome {
        self.persist_command_intent_v9_with_outcome(
            operation_id,
            intent,
            universal_intent,
            UniversalCommandArgs::Render(command_args),
            capacity_plan,
            timestamp_unix_ms,
        )
    }

    /// Persist one typed V9 whole-file Normalize observation.
    pub fn persist_waveform_normalize_intent(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: WaveformNormalizeIntentArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Result<JournalInsertReceipt, OperationJournalStoreError> {
        self.persist_render_command_intent(
            operation_id,
            intent,
            universal_intent,
            RenderCommandArgs::Normalize(command_args),
            capacity_plan,
            timestamp_unix_ms,
        )
    }

    /// Persist one typed V9 whole-file DuplicateDouble observation.
    pub fn persist_waveform_duplicate_double_intent(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: WaveformDuplicateDoubleIntentArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Result<JournalInsertReceipt, OperationJournalStoreError> {
        self.persist_render_command_intent(
            operation_id,
            intent,
            universal_intent,
            RenderCommandArgs::DuplicateDouble(command_args),
            capacity_plan,
            timestamp_unix_ms,
        )
    }

    /// Persist one typed V8 namespace command and return its compatibility receipt.
    pub fn persist_namespace_command_intent(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: NamespaceCommandArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Result<JournalInsertReceipt, OperationJournalStoreError> {
        self.persist_namespace_command_intent_with_outcome(
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
            timestamp_unix_ms,
        )
        .into_result()
        .map_err(JournalInsertFailure::into_error)
    }

    /// Persist one typed V8 namespace command with its exact write-stage outcome.
    pub fn persist_namespace_command_intent_with_outcome(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: NamespaceCommandArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> JournalInsertOutcome {
        self.persist_command_intent_v8_with_outcome(
            operation_id,
            intent,
            universal_intent,
            UniversalCommandArgs::Namespace(command_args),
            capacity_plan,
            timestamp_unix_ms,
        )
    }

    /// Persist one typed V7 in-place WAV edit and return its compatibility receipt.
    ///
    /// The edit arguments are bounded observations only. They do not retain a source lease,
    /// authorize replacement, reserve capacity, or perform filesystem work. An `Err` may be
    /// returned after COMMIT; callers that need the write-stage distinction should use
    /// [`Self::persist_waveform_edit_intent_with_outcome`].
    pub fn persist_waveform_edit_intent(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        edit_args: WaveformEditIntentArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Result<JournalInsertReceipt, OperationJournalStoreError> {
        self.persist_waveform_edit_intent_with_outcome(
            operation_id,
            intent,
            universal_intent,
            edit_args,
            capacity_plan,
            timestamp_unix_ms,
        )
        .into_result()
        .map_err(JournalInsertFailure::into_error)
    }

    /// Persist one typed V7 in-place WAV edit and expose its exact write-stage outcome.
    pub fn persist_waveform_edit_intent_with_outcome(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        edit_args: WaveformEditIntentArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> JournalInsertOutcome {
        if intent.actor != OperationActor::User || intent.kind != OperationKind::FileMutation {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from(
                        "schema-v7 waveform edit intent requires a user file-mutation operation",
                    ),
                },
                None,
                0,
                None,
            );
        }
        if universal_intent.command_kind() != super::model::UniversalCommandKind::DestructiveEdit {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from(
                        "schema-v7 waveform edit requires the DestructiveEdit command kind",
                    ),
                },
                None,
                0,
                None,
            );
        }
        if timestamp_unix_ms < 0 {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from("schema-v7 timestamps must be non-negative"),
                },
                None,
                0,
                None,
            );
        }
        if let Err(error) = edit_args.validate_against_participants(universal_intent.participants())
        {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                OperationJournalStoreError::InconsistentRecord {
                    reason: error.to_string(),
                },
                None,
                0,
                None,
            );
        }
        let record = StoredOperationRecord::new_v7_waveform_edit(
            operation_id,
            intent,
            universal_intent,
            edit_args,
            capacity_plan,
            timestamp_unix_ms,
        );
        self.insert_intent_with_outcome(&record)
    }

    /// Persist a user cancellation request against a current V3 or V5-V16 universal intent.
    ///
    /// The expected sequence is a compare-and-swap guard. A retry carrying the exact observation
    /// already stored for this operation is a read-only idempotent replay and returns the current
    /// receipt without allocating another sequence. All other stale, legacy, malformed, or
    /// unsupported rows remain unchanged. A V3 row becomes V4 while typed coordinator rows retain their
    /// schema so typed command and retry observations are preserved. The resulting record is
    /// intentionally non-terminal; it grants no cleanup, capacity-release, filesystem, or
    /// recovery execution authority.
    pub fn request_cancel_before_publish(
        &self,
        operation_id: OperationId,
        expected_sequence: i64,
        observation: CancellationRequestObservation,
    ) -> Result<JournalCancelRequestReceipt, OperationJournalStoreError> {
        if expected_sequence <= 0 {
            return Err(OperationJournalStoreError::InvalidExpectedSequence {
                expected: expected_sequence,
            });
        }
        observation.validate().map_err(|error| {
            OperationJournalStoreError::InvalidCancellationRequest {
                reason: error.to_string(),
            }
        })?;

        self.validate_namespace()?;
        self.connection.execute_batch("BEGIN EXCLUSIVE")?;
        let result =
            self.request_cancel_inside_transaction(operation_id, expected_sequence, observation);
        match result {
            Ok(CancelTransition::Replay(receipt)) => {
                let rollback = self.connection.execute_batch("ROLLBACK");
                if let Err(error) = rollback {
                    return Err(error.into());
                }
                self.validate_namespace()?;
                Ok(receipt)
            }
            Ok(CancelTransition::Updated { receipt, record }) => {
                if let Err(error) = self.validate_namespace_without_sidecars() {
                    let _ = self.connection.execute_batch("ROLLBACK");
                    return Err(error);
                }
                if let Err(error) = self.connection.execute_batch("COMMIT") {
                    let _ = self.connection.execute_batch("ROLLBACK");
                    return Err(error.into());
                }
                self.validate_namespace()?;
                let persisted = self.get(operation_id)?;
                if persisted.as_ref() != Some(&record) {
                    return Err(retained_error(
                        Some(operation_id),
                        String::from("committed cancellation request could not be read back"),
                    ));
                }
                Ok(receipt)
            }
            Err(error) => {
                let _ = self.connection.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    fn request_cancel_inside_transaction(
        &self,
        operation_id: OperationId,
        expected_sequence: i64,
        observation: CancellationRequestObservation,
    ) -> Result<CancelTransition, OperationJournalStoreError> {
        self.validate_namespace_without_sidecars()?;
        let Some(current) = self.read_row(operation_id)? else {
            return Err(OperationJournalStoreError::OperationNotFound { operation_id });
        };

        if matches!(
            current.record.schema_version,
            SCHEMA_V4
                | SCHEMA_V5
                | SCHEMA_V6
                | SCHEMA_V7
                | SCHEMA_V8
                | SCHEMA_V9
                | SCHEMA_V10
                | SCHEMA_V11
                | SCHEMA_V12
                | SCHEMA_V13
                | SCHEMA_V14
                | SCHEMA_V15
                | SCHEMA_V16
                | SCHEMA_V17
                | SCHEMA_V18
        ) && current.record.disposition == OperationDisposition::CancelRequestedBeforePublish
        {
            if current.record.cancellation_request() == Some(&observation) {
                return Ok(CancelTransition::Replay(JournalCancelRequestReceipt {
                    operation_id,
                    sequence: current.sequence,
                    encoded_bytes: current.encoded.len(),
                }));
            }
            return Err(OperationJournalStoreError::CancellationConflict { operation_id });
        }

        if current.sequence != expected_sequence {
            return Err(OperationJournalStoreError::StaleSequence {
                operation_id,
                expected: expected_sequence,
                actual: current.sequence,
            });
        }
        if !matches!(
            current.record.schema_version,
            SCHEMA_V3
                | SCHEMA_V5
                | SCHEMA_V6
                | SCHEMA_V7
                | SCHEMA_V8
                | SCHEMA_V9
                | SCHEMA_V10
                | SCHEMA_V11
                | SCHEMA_V12
                | SCHEMA_V13
                | SCHEMA_V14
                | SCHEMA_V15
                | SCHEMA_V16
                | SCHEMA_V17
                | SCHEMA_V18
        ) {
            return Err(OperationJournalStoreError::UnsupportedCancellationSchema {
                schema_version: current.record.schema_version,
            });
        }
        let external_reconciliation =
            matches!(current.record.schema_version, SCHEMA_V15 | SCHEMA_V16)
                && current.record.intent.kind == OperationKind::ExternalReconciliation;
        if current.record.intent.actor != OperationActor::User
            && !(external_reconciliation
                && current.record.intent.actor == OperationActor::ExternalFs)
        {
            return Err(OperationJournalStoreError::UnsupportedCancellationActor {
                actor: current.record.intent.actor,
            });
        }
        let eligible_disposition = match current.record.schema_version {
            SCHEMA_V3 => current.record.disposition == OperationDisposition::None,
            SCHEMA_V5 => matches!(
                current.record.disposition,
                OperationDisposition::None | OperationDisposition::RetryPending
            ),
            SCHEMA_V6 => matches!(
                current.record.disposition,
                OperationDisposition::None | OperationDisposition::RetryPending
            ),
            SCHEMA_V7 => matches!(
                current.record.disposition,
                OperationDisposition::None | OperationDisposition::RetryPending
            ),
            SCHEMA_V8 => matches!(
                current.record.disposition,
                OperationDisposition::None | OperationDisposition::RetryPending
            ),
            SCHEMA_V9 => matches!(
                current.record.disposition,
                OperationDisposition::None | OperationDisposition::RetryPending
            ),
            SCHEMA_V10 => matches!(
                current.record.disposition,
                OperationDisposition::None | OperationDisposition::RetryPending
            ),
            SCHEMA_V11 => matches!(
                current.record.disposition,
                OperationDisposition::None | OperationDisposition::RetryPending
            ),
            SCHEMA_V12 => matches!(
                current.record.disposition,
                OperationDisposition::None | OperationDisposition::RetryPending
            ),
            SCHEMA_V13 => matches!(
                current.record.disposition,
                OperationDisposition::None | OperationDisposition::RetryPending
            ),
            SCHEMA_V14 => matches!(
                current.record.disposition,
                OperationDisposition::None | OperationDisposition::RetryPending
            ),
            SCHEMA_V15 => matches!(
                current.record.disposition,
                OperationDisposition::None | OperationDisposition::RetryPending
            ),
            SCHEMA_V16 | SCHEMA_V17 | SCHEMA_V18 => matches!(
                current.record.disposition,
                OperationDisposition::None | OperationDisposition::RetryPending
            ),
            _ => false,
        };
        if current.record.phase != OperationPhase::IntentDurable || !eligible_disposition {
            let expected = match current.record.schema_version {
                SCHEMA_V5 => "V5 IntentDurable/None or RetryPending",
                SCHEMA_V6 => "V6 IntentDurable/None or RetryPending",
                SCHEMA_V7 => "V7 IntentDurable/None or RetryPending",
                SCHEMA_V8 => "V8 IntentDurable/None or RetryPending",
                SCHEMA_V9 => "V9 IntentDurable/None or RetryPending",
                SCHEMA_V10 => "V10 IntentDurable/None or RetryPending",
                SCHEMA_V11 => "V11 IntentDurable/None or RetryPending",
                SCHEMA_V12 => "V12 IntentDurable/None or RetryPending",
                SCHEMA_V13 => "V13 IntentDurable/None or RetryPending",
                SCHEMA_V14 => "V14 IntentDurable/None or RetryPending",
                SCHEMA_V15 => "V15 IntentDurable/None or RetryPending",
                SCHEMA_V16 | SCHEMA_V17 | SCHEMA_V18 => {
                    "Full initial intent / None or RetryPending"
                }
                _ => "V3 IntentDurable/None",
            };
            return Err(OperationJournalStoreError::UnsupportedCancellationState {
                reason: format!(
                    "expected {expected}, found {:?}/{:?}",
                    current.record.phase, current.record.disposition
                ),
            });
        }
        if observation.requested_unix_ms() < current.record.updated_unix_ms {
            return Err(OperationJournalStoreError::InvalidCancellationRequest {
                reason: format!(
                    "request timestamp {} precedes the stored update timestamp {}",
                    observation.requested_unix_ms(),
                    current.record.updated_unix_ms
                ),
            });
        }

        let next_record = current.record.with_cancellation_request(observation);
        let encoded = encode(&next_record)?;
        if encoded.len() > MAX_RECORD_BYTES {
            return Err(OperationJournalStoreError::InconsistentRecord {
                reason: format!("encoded record exceeds {MAX_RECORD_BYTES} bytes"),
            });
        }
        self.validate_namespace_without_sidecars()?;
        let sequence = self.next_sequence_inside_transaction()?;
        let changed = self.connection.execute(
            "UPDATE operation_journal
             SET sequence = ?1, record = ?2
             WHERE operation_id = ?3 AND sequence = ?4",
            params![
                sequence,
                encoded.as_slice(),
                operation_id.to_string(),
                expected_sequence
            ],
        )?;
        if changed != 1 {
            return Err(OperationJournalStoreError::StaleSequence {
                operation_id,
                expected: expected_sequence,
                actual: current.sequence,
            });
        }
        self.validate_namespace_without_sidecars()?;
        Ok(CancelTransition::Updated {
            receipt: JournalCancelRequestReceipt {
                operation_id,
                sequence,
                encoded_bytes: encoded.len(),
            },
            record: next_record,
        })
    }

    /// Persist the next typed V5-V16 retry observation using a sequence compare-and-swap.
    ///
    /// An identical observation already stored for the operation is a read-only idempotent
    /// replay, even when the caller still holds an older sequence. A changed observation must
    /// carry the current sequence and exactly the next attempt number. This method stores no live
    /// lease authority and does not mutate capacity, filesystem, or participant state.
    pub fn record_retry_pending(
        &self,
        operation_id: OperationId,
        expected_sequence: i64,
        observation: RetryObservation,
    ) -> Result<JournalRetryReceipt, OperationJournalStoreError> {
        super::retry_telemetry::observe(
            super::retry_telemetry::JournalRetryCall::RetryPending,
            || self.record_retry_pending_inner(operation_id, expected_sequence, observation),
        )
    }

    fn record_retry_pending_inner(
        &self,
        operation_id: OperationId,
        expected_sequence: i64,
        observation: RetryObservation,
    ) -> Result<JournalRetryReceipt, OperationJournalStoreError> {
        if expected_sequence <= 0 {
            return Err(OperationJournalStoreError::InvalidExpectedSequence {
                expected: expected_sequence,
            });
        }
        observation.validate().map_err(|error| {
            OperationJournalStoreError::InvalidRetryObservation {
                reason: error.to_string(),
            }
        })?;

        self.validate_namespace()?;
        self.connection.execute_batch("BEGIN EXCLUSIVE")?;
        let result =
            self.record_retry_inside_transaction(operation_id, expected_sequence, observation);
        match result {
            Ok(RetryTransition::Replay(receipt)) => {
                let rollback = self.connection.execute_batch("ROLLBACK");
                if let Err(error) = rollback {
                    return Err(error.into());
                }
                self.validate_namespace()?;
                Ok(receipt)
            }
            Ok(RetryTransition::Updated { receipt, record }) => {
                if let Err(error) = self.validate_namespace_without_sidecars() {
                    let _ = self.connection.execute_batch("ROLLBACK");
                    return Err(error);
                }
                if let Err(error) = self.connection.execute_batch("COMMIT") {
                    let _ = self.connection.execute_batch("ROLLBACK");
                    return Err(error.into());
                }
                self.validate_namespace()?;
                let persisted = self.get(operation_id)?;
                if persisted.as_ref() != Some(&record) {
                    return Err(retained_error(
                        Some(operation_id),
                        String::from("committed retry observation could not be read back"),
                    ));
                }
                Ok(receipt)
            }
            Err(error) => {
                let _ = self.connection.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    /// Persist a post-intent capacity retry and classify the resulting status from positive,
    /// durable incomplete staging evidence. The V1-V16 operation codec keeps its RetryPending
    /// retry lease; a validated chunk checkpoint projects PartialNeedsRetry on recovery.
    pub(crate) fn record_post_intent_chunk_exhaustion(
        &self,
        operation_id: OperationId,
        expected_sequence: i64,
        observation: RetryObservation,
    ) -> Result<(JournalRetryReceipt, OperationDisposition), OperationJournalStoreError> {
        super::retry_telemetry::observe(
            super::retry_telemetry::JournalRetryCall::ChunkExhaustion,
            || {
                self.record_post_intent_chunk_exhaustion_inner(
                    operation_id,
                    expected_sequence,
                    observation,
                )
            },
        )
    }

    fn record_post_intent_chunk_exhaustion_inner(
        &self,
        operation_id: OperationId,
        expected_sequence: i64,
        observation: RetryObservation,
    ) -> Result<(JournalRetryReceipt, OperationDisposition), OperationJournalStoreError> {
        if observation.error_class() != super::coordinator::RetryErrorClass::CapacityUnavailable {
            return Err(OperationJournalStoreError::InvalidRetryObservation {
                reason: String::from("chunk exhaustion requires CapacityUnavailable"),
            });
        }
        // Validate every bounded checkpoint before changing the operation. A malformed row is
        // retained for audit; it must not be interpreted as a clean RetryPending operation.
        let partial = self.has_valid_chunk_staging(operation_id)?;
        let receipt = self.record_retry_pending(operation_id, expected_sequence, observation)?;
        let persisted = self
            .get(operation_id)?
            .ok_or_else(|| OperationJournalStoreError::OperationNotFound { operation_id })?;
        if persisted.disposition != OperationDisposition::RetryPending
            || self.has_valid_chunk_staging(operation_id)? != partial
        {
            return Err(OperationJournalStoreError::ChunkStaging {
                reason: String::from("chunk exhaustion status changed during readback"),
            });
        }
        Ok((
            receipt,
            if partial {
                OperationDisposition::PartialNeedsRetry
            } else {
                OperationDisposition::RetryPending
            },
        ))
    }

    fn record_retry_inside_transaction(
        &self,
        operation_id: OperationId,
        expected_sequence: i64,
        observation: RetryObservation,
    ) -> Result<RetryTransition, OperationJournalStoreError> {
        self.validate_namespace_without_sidecars()?;
        let Some(current) = self.read_row(operation_id)? else {
            return Err(OperationJournalStoreError::OperationNotFound { operation_id });
        };

        if current.record.disposition == OperationDisposition::CancelRequestedBeforePublish {
            return Err(OperationJournalStoreError::RetryObservationConflict { operation_id });
        }
        if matches!(
            current.record.schema_version,
            SCHEMA_V5
                | SCHEMA_V6
                | SCHEMA_V7
                | SCHEMA_V8
                | SCHEMA_V9
                | SCHEMA_V10
                | SCHEMA_V11
                | SCHEMA_V12
                | SCHEMA_V13
                | SCHEMA_V14
                | SCHEMA_V15
                | SCHEMA_V16
                | SCHEMA_V17
                | SCHEMA_V18
        ) && current.record.disposition == OperationDisposition::RetryPending
            && current.record.retry_observation() == Some(&observation)
        {
            return Ok(RetryTransition::Replay(JournalRetryReceipt {
                operation_id,
                sequence: current.sequence,
                encoded_bytes: current.encoded.len(),
            }));
        }

        if current.sequence != expected_sequence {
            return Err(OperationJournalStoreError::StaleSequence {
                operation_id,
                expected: expected_sequence,
                actual: current.sequence,
            });
        }
        if !matches!(
            current.record.schema_version,
            SCHEMA_V5
                | SCHEMA_V6
                | SCHEMA_V7
                | SCHEMA_V8
                | SCHEMA_V9
                | SCHEMA_V10
                | SCHEMA_V11
                | SCHEMA_V12
                | SCHEMA_V13
                | SCHEMA_V14
                | SCHEMA_V15
                | SCHEMA_V16
                | SCHEMA_V17
                | SCHEMA_V18
        ) {
            return Err(OperationJournalStoreError::UnsupportedRetrySchema {
                schema_version: current.record.schema_version,
            });
        }
        if current.record.phase != OperationPhase::IntentDurable
            || !matches!(
                current.record.disposition,
                OperationDisposition::None | OperationDisposition::RetryPending
            )
        {
            return Err(OperationJournalStoreError::UnsupportedRetryState {
                reason: format!(
                    "expected V5/V6/V7/V8/V9/V10/V11/V12/V13/V14/V15/V16 IntentDurable/None or RetryPending, found {:?}/{:?}",
                    current.record.phase, current.record.disposition
                ),
            });
        }
        let expected_attempt = current
            .record
            .retry_observation()
            .map_or(1, |previous| previous.attempt().checked_add(1).unwrap_or(0));
        if expected_attempt == 0 || observation.attempt() != expected_attempt {
            return Err(OperationJournalStoreError::InvalidRetryObservation {
                reason: format!(
                    "retry attempt must advance from {} to exactly {}",
                    current
                        .record
                        .retry_observation()
                        .map_or(0, RetryObservation::attempt),
                    expected_attempt
                ),
            });
        }
        if observation.requested_unix_ms() < current.record.updated_unix_ms {
            return Err(OperationJournalStoreError::InvalidRetryObservation {
                reason: format!(
                    "retry request timestamp {} precedes the stored update timestamp {}",
                    observation.requested_unix_ms(),
                    current.record.updated_unix_ms
                ),
            });
        }

        let next_record = current.record.with_retry_pending(observation);
        let encoded = encode(&next_record)?;
        if encoded.len() > MAX_RECORD_BYTES {
            return Err(OperationJournalStoreError::InconsistentRecord {
                reason: format!("encoded record exceeds {MAX_RECORD_BYTES} bytes"),
            });
        }
        self.validate_namespace_without_sidecars()?;
        let sequence = self.next_sequence_inside_transaction()?;
        let changed = self.connection.execute(
            "UPDATE operation_journal
             SET sequence = ?1, record = ?2
             WHERE operation_id = ?3 AND sequence = ?4",
            params![
                sequence,
                encoded.as_slice(),
                operation_id.to_string(),
                expected_sequence
            ],
        )?;
        if changed != 1 {
            return Err(OperationJournalStoreError::StaleSequence {
                operation_id,
                expected: expected_sequence,
                actual: current.sequence,
            });
        }
        self.validate_namespace_without_sidecars()?;
        Ok(RetryTransition::Updated {
            receipt: JournalRetryReceipt {
                operation_id,
                sequence,
                encoded_bytes: encoded.len(),
            },
            record: next_record,
        })
    }

    /// Insert one internally consistent durable intent without changing any later phase.
    /// As a compatibility adapter, an `Err` may be returned after COMMIT and therefore does not
    /// prove that the operation row is absent. Claim or admission code should use
    /// [`Self::insert_intent_with_outcome`] when it needs the write-stage observation.
    pub fn insert_intent(
        &self,
        record: &StoredOperationRecord,
    ) -> Result<JournalInsertReceipt, OperationJournalStoreError> {
        self.insert_intent_with_outcome(record)
            .into_result()
            .map_err(JournalInsertFailure::into_error)
    }

    /// Atomically settle one validated initial ordinary claim set with its legacy intent row.
    ///
    /// The provider lease is pinned before the transaction starts.  Confirmed pre-commit
    /// rollback releases it; every ambiguous or post-commit outcome keeps the owner-thread token
    /// retained for later audit/reconciliation.  This remains crate-private and does not enable
    /// the production admission gate or treat full-progress observations as authority.
    #[allow(dead_code)]
    pub(crate) fn settle_initial_claims(
        &self,
        record: &StoredOperationRecord,
        provider: &CapacityProvider,
        claims: &AdmissionClaimInventory,
    ) -> Result<InitialClaimSettlement, InitialClaimSettlementError> {
        validate_intent_record(record).map_err(InitialClaimSettlementError::Store)?;
        let full_plan =
            full_capacity_plan_for_record(record).map_err(InitialClaimSettlementError::Store)?;
        claims.validate_against(&full_plan)?;
        ordinary_claims::validate_storage_values(claims)
            .map_err(|error| InitialClaimSettlementError::ClaimStorage(error.to_string()))?;
        let lease = provider.admit(&full_plan, claims)?;
        self.settle_initial_claims_with_lease(record, claims, provider, lease)
    }

    /// Settle an already-admitted owner-thread lease against one immutable initial intent.
    ///
    /// The caller owns the single provisional admission. This method only pins that lease,
    /// persists the intent and ordinary rows atomically, and applies the existing committed,
    /// rollback, or uncertain outcome rules; it never calls the provider a second time.
    pub(crate) fn settle_initial_claims_with_lease(
        &self,
        record: &StoredOperationRecord,
        claims: &AdmissionClaimInventory,
        provider: &CapacityProvider,
        lease: ProvisionalCapacityLease,
    ) -> Result<InitialClaimSettlement, InitialClaimSettlementError> {
        self.settle_initial_claims_with_inventory(record, claims, provider, lease, None)
    }

    fn settle_initial_claims_with_inventory(
        &self,
        record: &StoredOperationRecord,
        claims: &AdmissionClaimInventory,
        provider: &CapacityProvider,
        mut lease: ProvisionalCapacityLease,
        inventory: Option<&super::database_claims::DatabaseClaimInventory>,
    ) -> Result<InitialClaimSettlement, InitialClaimSettlementError> {
        validate_intent_record(record).map_err(InitialClaimSettlementError::Store)?;
        let full_plan =
            full_capacity_plan_for_record(record).map_err(InitialClaimSettlementError::Store)?;
        claims.validate_against(&full_plan)?;
        ordinary_claims::validate_storage_values(claims)
            .map_err(|error| InitialClaimSettlementError::ClaimStorage(error.to_string()))?;
        if let Some(inventory) = inventory {
            inventory
                .validate_record(record)
                .and_then(|()| inventory.validate_against(&full_plan, claims))
                .map_err(|error| {
                    InitialClaimSettlementError::Store(database_claims::error(error))
                })?;
            if inventory.header().profile != self.profile_guard.profile_root_identity()
                || inventory.header().acquisition != self.profile_guard.owner_identity()
                || inventory.header().ownership_generation
                    != self.profile_guard.ownership_generation()
            {
                return Err(InitialClaimSettlementError::Store(database_claims::error(
                    "current profile binding mismatch",
                )));
            }
        }
        let encoded = encode(record)
            .map_err(OperationJournalStoreError::from)
            .map_err(InitialClaimSettlementError::Store)?;
        lease
            .validate_against(provider, &full_plan, claims)
            .map_err(InitialClaimSettlementError::Capacity)?;
        lease
            .pin_for_persistence(record.operation_id)
            .map_err(InitialClaimSettlementError::Capacity)?;

        let outcome = self.persist_record_with_encoded_outcome_and_database_claims(
            record,
            &encoded,
            Some(claims),
            inventory,
        );
        let retain = match &outcome {
            JournalInsertOutcome::Committed(_) => false,
            JournalInsertOutcome::Failed(failure) => matches!(
                failure.stage(),
                JournalInsertWriteStage::RollbackNotConfirmed
                    | JournalInsertWriteStage::CommitOutcomeUnknown
                    | JournalInsertWriteStage::CommittedButNotAcknowledged
            ),
        };
        if matches!(&outcome, JournalInsertOutcome::Committed(_)) {
            if let Err(source) = lease.mark_durable(record.operation_id) {
                return Err(InitialClaimSettlementError::OutcomeRetained {
                    outcome,
                    lease,
                    source,
                });
            }
            return Ok(InitialClaimSettlement {
                outcome,
                lease: Some(lease),
            });
        }
        if retain {
            return Ok(InitialClaimSettlement {
                outcome,
                lease: Some(lease),
            });
        }

        let lease = match lease.release_after_confirmed_rollback(record.operation_id) {
            Ok(()) => None,
            Err(source) => {
                return Err(InitialClaimSettlementError::OutcomeRetained {
                    outcome,
                    lease,
                    source,
                });
            }
        };
        Ok(InitialClaimSettlement { outcome, lease })
    }

    /// Persist and reconcile one explicit owner-thread logical release. The immutable receipt is
    /// committed and read back before the provider ledger is decremented; a provider failure
    /// therefore returns the receipt while retaining the original charge for retry.
    #[allow(dead_code)]
    pub(crate) fn release_ordinary_claim(
        &self,
        request: OrdinaryClaimReleaseRequest,
        provider: &CapacityProvider,
    ) -> Result<OrdinaryClaimReleaseReceipt, OrdinaryClaimReleaseError> {
        let receipt = self.persist_ordinary_claim_release(request)?;
        if let Err(source) = provider.release_claim(&receipt) {
            return Err(OrdinaryClaimReleaseError::ProviderRetained { receipt, source });
        }
        Ok(receipt)
    }

    #[allow(dead_code)]
    fn persist_ordinary_claim_release(
        &self,
        request: OrdinaryClaimReleaseRequest,
    ) -> Result<OrdinaryClaimReleaseReceipt, OrdinaryClaimReleaseError> {
        self.validate_namespace()
            .map_err(|source| OrdinaryClaimReleaseError::Persistence { source })?;
        #[cfg(test)]
        if let Some(ReleaseTestDirective::Fail(error))
        | Some(ReleaseTestDirective::FailAndLeaveForRollback(error)) =
            self.release_test_directive(ReleaseTestBoundary::BeforeBegin)
        {
            return Err(OrdinaryClaimReleaseError::Persistence { source: error });
        }
        self.connection
            .execute_batch("BEGIN EXCLUSIVE")
            .map_err(|error| OrdinaryClaimReleaseError::Persistence {
                source: error.into(),
            })?;
        let transition = match self.release_ordinary_claim_inside_transaction(request) {
            Ok(transition) => transition,
            Err(error) => return Err(self.rollback_ordinary_claim_release(error)),
        };
        if let Err(source) = self.validate_namespace_without_sidecars() {
            return Err(self.rollback_ordinary_claim_release(
                OrdinaryClaimReleaseError::Persistence { source },
            ));
        }
        #[cfg(test)]
        if let Some(directive) =
            self.release_test_directive(ReleaseTestBoundary::BeforePrecommitValidation)
        {
            match directive {
                ReleaseTestDirective::Fail(error)
                | ReleaseTestDirective::FailAndLeaveForRollback(error) => {
                    return Err(self.rollback_ordinary_claim_release(
                        OrdinaryClaimReleaseError::Persistence { source: error },
                    ));
                }
                ReleaseTestDirective::Continue | ReleaseTestDirective::ExecuteThenFail(_) => {}
            }
        }
        #[cfg(test)]
        let commit_directive = self.release_test_directive(ReleaseTestBoundary::Commit);
        #[cfg(test)]
        if let Some(ReleaseTestDirective::Fail(error)) = commit_directive {
            let rollback_error = self.rollback_ordinary_claim_release_transaction();
            return Err(OrdinaryClaimReleaseError::PersistenceUncertain {
                source: rollback_error.unwrap_or(error),
            });
        }
        #[cfg(test)]
        if let Some(ReleaseTestDirective::ExecuteThenFail(error)) = commit_directive {
            let commit_result = self.connection.execute_batch("COMMIT");
            let rollback_error = self.rollback_ordinary_claim_release_transaction();
            if let Err(commit_error) = commit_result {
                return Err(OrdinaryClaimReleaseError::PersistenceUncertain {
                    source: rollback_error.unwrap_or_else(|| commit_error.into()),
                });
            }
            return Err(OrdinaryClaimReleaseError::PersistenceUncertain {
                source: rollback_error.unwrap_or(error),
            });
        }
        if let Err(error) = self.connection.execute_batch("COMMIT") {
            let rollback_error = self.rollback_ordinary_claim_release_transaction();
            return Err(OrdinaryClaimReleaseError::PersistenceUncertain {
                source: rollback_error.unwrap_or_else(|| error.into()),
            });
        }
        #[cfg(test)]
        if let Some(ReleaseTestDirective::Fail(error)) =
            self.release_test_directive(ReleaseTestBoundary::AfterCommit)
        {
            return Err(OrdinaryClaimReleaseError::PersistenceUncertain { source: error });
        }
        if let Err(source) = self.validate_namespace() {
            return Err(OrdinaryClaimReleaseError::PersistenceUncertain { source });
        }
        let receipt = match transition {
            OrdinaryClaimReleaseTransition::Replay(receipt)
            | OrdinaryClaimReleaseTransition::Inserted(receipt) => receipt,
        };
        let persisted = ordinary_claims::read_release(&self.connection, request.claim_id())
            .map_err(|error| OrdinaryClaimReleaseError::PersistenceUncertain {
                source: ordinary_claim_read_error(error),
            })?;
        if persisted
            != Some(ordinary_claims::OrdinaryClaimReleaseRecord::new(
                request.claim_id(),
                request.operation_id(),
                request.release_id(),
            ))
        {
            return Err(OrdinaryClaimReleaseError::PersistenceUncertain {
                source: retained_error(
                    Some(request.operation_id()),
                    String::from("ordinary claim release readback changed after commit"),
                ),
            });
        }
        Ok(receipt)
    }

    #[allow(dead_code)]
    fn release_ordinary_claim_inside_transaction(
        &self,
        request: OrdinaryClaimReleaseRequest,
    ) -> Result<OrdinaryClaimReleaseTransition, OrdinaryClaimReleaseError> {
        self.validate_namespace_without_sidecars()
            .map_err(|source| OrdinaryClaimReleaseError::Persistence { source })?;
        let retained: bool = self.connection.query_row("SELECT EXISTS(SELECT 1 FROM operation_journal_recording_chunks WHERE operation_id=?1)", params![request.operation_id().to_string()], |r| r.get(0)).map_err(|source| OrdinaryClaimReleaseError::Persistence { source: source.into() })?;
        if retained {
            return Err(OrdinaryClaimReleaseError::Persistence { source: OperationJournalStoreError::RecordingRetention { reason: "retained recording prefix forbids capacity release until qualified retirement is implemented".into() } });
        }
        let bound: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM operation_journal_database_claim_slice WHERE claim_id=?1)",
            [request.claim_id().to_string()], |r| r.get(0))
            .map_err(|source| OrdinaryClaimReleaseError::Persistence { source: source.into() })?;
        if bound {
            return Err(OrdinaryClaimReleaseError::Persistence {
                source: database_claims::error(
                    "database slices remain retained until exact attempt settlement and allocation retirement",
                ),
            });
        }
        let unresolved:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM operation_journal_database_attempt WHERE operation_id=?1)", [request.operation_id().to_string()], |r|r.get(0)).map_err(|source|OrdinaryClaimReleaseError::Persistence{source:source.into()})?;
        if unresolved {
            return Err(OrdinaryClaimReleaseError::Persistence {
                source: database_claims::error(
                    "retained attempt forbids ordinary release until genuine settlement/retirement",
                ),
            });
        }
        release_ordinary_claim_transition_inside_transaction(&self.connection, request)
    }

    #[allow(dead_code)]
    fn rollback_ordinary_claim_release(
        &self,
        error: OrdinaryClaimReleaseError,
    ) -> OrdinaryClaimReleaseError {
        match self.rollback_ordinary_claim_release_transaction() {
            Some(source) => OrdinaryClaimReleaseError::PersistenceUncertain { source },
            None => error,
        }
    }

    #[allow(dead_code)]
    fn rollback_ordinary_claim_release_transaction(&self) -> Option<OperationJournalStoreError> {
        #[cfg(test)]
        if let Some(directive) = self.release_test_directive(ReleaseTestBoundary::Rollback) {
            match directive {
                ReleaseTestDirective::Fail(error)
                | ReleaseTestDirective::FailAndLeaveForRollback(error) => return Some(error),
                ReleaseTestDirective::ExecuteThenFail(error) => {
                    let _ = self.connection.execute_batch("ROLLBACK");
                    return Some(error);
                }
                ReleaseTestDirective::Continue => {}
            }
        }
        self.connection
            .execute_batch("ROLLBACK")
            .err()
            .map(OperationJournalStoreError::from)
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn set_release_test_hook<F>(&self, hook: F)
    where
        F: FnMut(ReleaseTestBoundary) -> ReleaseTestDirective + Send + 'static,
    {
        *self
            .release_test_hook
            .lock()
            .expect("release test hook mutex") = Some(Box::new(hook));
    }

    #[cfg(test)]
    fn release_test_directive(
        &self,
        boundary: ReleaseTestBoundary,
    ) -> Option<ReleaseTestDirective> {
        let mut hook = self
            .release_test_hook
            .lock()
            .expect("release test hook mutex");
        let directive = hook.as_mut().map(|hook| hook(boundary));
        if matches!(
            &directive,
            Some(ReleaseTestDirective::Fail(_) | ReleaseTestDirective::ExecuteThenFail(_))
        ) {
            *hook = None;
        }
        directive
    }

    /// Insert one durable intent and expose the exact write-stage outcome.
    ///
    /// This is the typed counterpart to [`Self::insert_intent`]. The returned receipt is exposed
    /// only after commit, full post-commit namespace validation, and exact readback. A failed
    /// outcome retains the original store error and never asserts that the operation id is absent.
    pub fn insert_intent_with_outcome(
        &self,
        record: &StoredOperationRecord,
    ) -> JournalInsertOutcome {
        let operation_id = record.operation_id;
        if let Err(error) = validate_intent_record(record) {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                error,
                None,
                0,
                None,
            );
        }
        let encoded = match encode(record) {
            Ok(encoded) => encoded,
            Err(error) => {
                return failed_insert(
                    operation_id,
                    JournalInsertWriteStage::NoMutationAttempted,
                    error.into(),
                    None,
                    0,
                    None,
                );
            }
        };
        self.persist_record_with_encoded_outcome(record, &encoded)
    }

    fn persist_record_with_encoded_outcome(
        &self,
        record: &StoredOperationRecord,
        encoded: &[u8],
    ) -> JournalInsertOutcome {
        self.persist_record_with_encoded_outcome_and_claims(record, encoded, None)
    }

    fn persist_record_with_encoded_outcome_and_claims(
        &self,
        record: &StoredOperationRecord,
        encoded: &[u8],
        claims: Option<&AdmissionClaimInventory>,
    ) -> JournalInsertOutcome {
        self.persist_record_with_encoded_outcome_and_database_claims(record, encoded, claims, None)
    }

    fn persist_record_with_encoded_outcome_and_database_claims(
        &self,
        record: &StoredOperationRecord,
        encoded: &[u8],
        claims: Option<&AdmissionClaimInventory>,
        inventory: Option<&super::database_claims::DatabaseClaimInventory>,
    ) -> JournalInsertOutcome {
        let operation_id = record.operation_id;
        if let Err(error) = self.validate_namespace() {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                error,
                None,
                encoded.len(),
                None,
            );
        }
        if let Err(error) = self.prepare_recording_initial_policy(record) {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                error,
                None,
                encoded.len(),
                None,
            );
        }
        #[cfg(test)]
        if let Some(InsertTestDirective::Fail(error)) =
            self.insert_test_directive(InsertTestBoundary::BeforeBegin)
        {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                error,
                None,
                encoded.len(),
                None,
            );
        }
        if let Err(error) = self.connection.execute_batch("BEGIN EXCLUSIVE") {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::NoMutationAttempted,
                error.into(),
                None,
                encoded.len(),
                None,
            );
        }

        let receipt = match self.insert_inside_transaction(record, encoded) {
            Ok(receipt) => receipt,
            Err(error) => {
                return self.rollback_insert_failure(operation_id, error, None, encoded.len());
            }
        };
        if let Err(error) = self.reserve_recording_initial_inside_transaction(record) {
            return self.rollback_insert_failure(
                operation_id,
                error,
                Some(receipt.sequence),
                encoded.len(),
            );
        }
        if let Some(claims) = claims {
            if let Err(error) = ordinary_claims::insert_rows_inside_transaction(
                &self.connection,
                operation_id,
                claims,
            )
            .and_then(|()| {
                ordinary_claims::verify_rows_inside_transaction(
                    &self.connection,
                    operation_id,
                    claims,
                )
            }) {
                return self.rollback_insert_failure(
                    operation_id,
                    ordinary_claim_write_error(error),
                    Some(receipt.sequence),
                    encoded.len(),
                );
            }
        }

        if let Some(inventory) = inventory {
            if let Err(error) = database_claims::insert(&self.connection, operation_id, inventory) {
                return self.rollback_insert_failure(
                    operation_id,
                    error,
                    Some(receipt.sequence),
                    encoded.len(),
                );
            }
        }

        #[cfg(test)]
        if let Some(directive) =
            self.insert_test_directive(InsertTestBoundary::BeforePrecommitValidation)
        {
            match directive {
                InsertTestDirective::Fail(error)
                | InsertTestDirective::FailAndLeaveForRollback(error) => {
                    return self.rollback_insert_failure(
                        operation_id,
                        error,
                        Some(receipt.sequence),
                        encoded.len(),
                    );
                }
                InsertTestDirective::Continue | InsertTestDirective::ExecuteThenFail(_) => {}
            }
        }
        if let Err(error) = self.validate_namespace_without_sidecars() {
            return self.rollback_insert_failure(
                operation_id,
                error,
                Some(receipt.sequence),
                encoded.len(),
            );
        }

        #[cfg(test)]
        let commit_directive = self.insert_test_directive(InsertTestBoundary::Commit);
        #[cfg(test)]
        if let Some(InsertTestDirective::Fail(error)) = commit_directive {
            let rollback_error = self.rollback_insert_transaction();
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::CommitOutcomeUnknown,
                error,
                Some(receipt.sequence),
                encoded.len(),
                rollback_error,
            );
        }
        #[cfg(test)]
        if let Some(InsertTestDirective::ExecuteThenFail(error)) = commit_directive {
            let commit_result = self.connection.execute_batch("COMMIT");
            let rollback_error = self.rollback_insert_transaction();
            if let Err(commit_error) = commit_result {
                return failed_insert(
                    operation_id,
                    JournalInsertWriteStage::CommitOutcomeUnknown,
                    commit_error.into(),
                    Some(receipt.sequence),
                    encoded.len(),
                    rollback_error,
                );
            }
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::CommitOutcomeUnknown,
                error,
                Some(receipt.sequence),
                encoded.len(),
                rollback_error,
            );
        }
        if let Err(error) = self.connection.execute_batch("COMMIT") {
            let rollback_error = self.rollback_insert_transaction();
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::CommitOutcomeUnknown,
                error.into(),
                Some(receipt.sequence),
                encoded.len(),
                rollback_error,
            );
        }

        #[cfg(test)]
        if let Some(InsertTestDirective::Fail(error)) =
            self.insert_test_directive(InsertTestBoundary::AfterCommit)
        {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::CommittedButNotAcknowledged,
                error,
                Some(receipt.sequence),
                encoded.len(),
                None,
            );
        }
        if let Err(error) = self.validate_namespace() {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::CommittedButNotAcknowledged,
                error,
                Some(receipt.sequence),
                encoded.len(),
                None,
            );
        }
        let persisted = match self.get(operation_id) {
            Ok(persisted) => persisted,
            Err(error) => {
                return failed_insert(
                    operation_id,
                    JournalInsertWriteStage::CommittedButNotAcknowledged,
                    error,
                    Some(receipt.sequence),
                    encoded.len(),
                    None,
                );
            }
        };
        if persisted.as_ref() != Some(record) {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::CommittedButNotAcknowledged,
                retained_error(
                    Some(operation_id),
                    String::from("committed journal row could not be read back identically"),
                ),
                Some(receipt.sequence),
                encoded.len(),
                None,
            );
        }
        if let Err(error) = self.verify_recording_initial_readback(record) {
            return failed_insert(
                operation_id,
                JournalInsertWriteStage::CommittedButNotAcknowledged,
                error,
                Some(receipt.sequence),
                encoded.len(),
                None,
            );
        }
        if let Some(claims) = claims {
            if let Err(error) = ordinary_claims::verify_rows_inside_transaction(
                &self.connection,
                operation_id,
                claims,
            ) {
                return failed_insert(
                    operation_id,
                    JournalInsertWriteStage::CommittedButNotAcknowledged,
                    ordinary_claim_write_error(error),
                    Some(receipt.sequence),
                    encoded.len(),
                    None,
                );
            }
        }
        if let Some(inventory) = inventory {
            if let Err(error) = database_claims::verify(&self.connection, operation_id, inventory) {
                return failed_insert(
                    operation_id,
                    JournalInsertWriteStage::CommittedButNotAcknowledged,
                    error,
                    Some(receipt.sequence),
                    encoded.len(),
                    None,
                );
            }
        }
        JournalInsertOutcome::Committed(receipt)
    }

    fn rollback_insert_failure(
        &self,
        operation_id: OperationId,
        error: OperationJournalStoreError,
        attempted_sequence: Option<i64>,
        encoded_bytes: usize,
    ) -> JournalInsertOutcome {
        let rollback_error = self.rollback_insert_transaction();
        let stage = if rollback_error.is_some() {
            JournalInsertWriteStage::RollbackNotConfirmed
        } else {
            JournalInsertWriteStage::RolledBackBeforeCommit
        };
        failed_insert(
            operation_id,
            stage,
            error,
            attempted_sequence,
            encoded_bytes,
            rollback_error,
        )
    }

    fn rollback_insert_transaction(&self) -> Option<Box<OperationJournalStoreError>> {
        #[cfg(test)]
        if let Some(directive) = self.insert_test_directive(InsertTestBoundary::Rollback) {
            match directive {
                InsertTestDirective::Fail(error)
                | InsertTestDirective::FailAndLeaveForRollback(error) => {
                    return Some(Box::new(error));
                }
                InsertTestDirective::ExecuteThenFail(error) => {
                    let _ = self.connection.execute_batch("ROLLBACK");
                    return Some(Box::new(error));
                }
                InsertTestDirective::Continue => {}
            }
        }
        self.connection
            .execute_batch("ROLLBACK")
            .err()
            .map(|error| Box::new(OperationJournalStoreError::from(error)))
    }

    #[cfg(test)]
    pub(crate) fn set_insert_test_hook<F>(&self, hook: F)
    where
        F: FnMut(InsertTestBoundary) -> InsertTestDirective + Send + 'static,
    {
        *self
            .insert_test_hook
            .lock()
            .expect("insert test hook mutex") = Some(Box::new(hook));
    }

    #[cfg(test)]
    fn insert_test_directive(&self, boundary: InsertTestBoundary) -> Option<InsertTestDirective> {
        let mut hook = self
            .insert_test_hook
            .lock()
            .expect("insert test hook mutex");
        let directive = hook.as_mut().map(|hook| hook(boundary));
        if matches!(
            &directive,
            Some(InsertTestDirective::Fail(_) | InsertTestDirective::ExecuteThenFail(_))
        ) {
            *hook = None;
        }
        directive
    }

    fn insert_inside_transaction(
        &self,
        record: &StoredOperationRecord,
        encoded: &[u8],
    ) -> Result<JournalInsertReceipt, OperationJournalStoreError> {
        self.validate_namespace_without_sidecars()?;
        let operation_id = record.operation_id;
        let key = operation_id.to_string();
        let (legacy_present, full_progress_present) =
            self.operation_family_presence(operation_id)?;
        if legacy_present && full_progress_present {
            return Err(audit_error(
                &self.database_path,
                format!("operation id {operation_id} is present in both journal families"),
            ));
        }
        if legacy_present || full_progress_present {
            return Err(OperationJournalStoreError::DuplicateOperation { operation_id });
        }
        let sequence = self.next_sequence_inside_transaction()?;
        self.connection.execute(
            "INSERT INTO operation_journal (operation_id, sequence, record)
             VALUES (?1, ?2, ?3)",
            params![key, sequence, encoded],
        )?;
        Ok(JournalInsertReceipt {
            operation_id,
            sequence,
            encoded_bytes: encoded.len(),
        })
    }

    fn next_sequence_inside_transaction(&self) -> Result<i64, OperationJournalStoreError> {
        let previous_sequence = self.connection.query_row(
            "SELECT MAX(sequence) FROM (
                 SELECT sequence FROM operation_journal
                 UNION ALL
                 SELECT sequence FROM operation_journal_full_progress
                 UNION ALL
                 SELECT sequence FROM operation_journal_staging_checkpoints
                 UNION ALL
                 SELECT sequence FROM operation_journal_chunk_staging
                 UNION ALL
                 SELECT sequence FROM operation_journal_recording_chunks
             )",
            [],
            |row| row.get::<_, Option<i64>>(0),
        )?;
        if previous_sequence.is_some_and(|sequence| sequence <= 0) {
            return Err(audit_error(
                &self.database_path,
                "stored journal-observation sequence is not positive",
            ));
        }
        previous_sequence
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(OperationJournalStoreError::SequenceExhausted)
    }

    /// Read one row with the same bounded key, sequence, blob, codec, and consistency checks used
    /// by point reads. This helper deliberately performs no namespace or sidecar inspection so it
    /// can run inside the store's exclusive transaction.
    fn read_row(
        &self,
        operation_id: OperationId,
    ) -> Result<Option<JournalRow>, OperationJournalStoreError> {
        #[cfg(test)]
        POINT_ROW_MATERIALIZED_BYTES.with(|bytes| bytes.set(0));
        let (legacy_present, full_progress_present) =
            self.operation_family_presence(operation_id)?;
        if legacy_present && full_progress_present {
            return Err(audit_error(
                &self.database_path,
                format!("operation id {operation_id} is present in both journal families"),
            ));
        }
        if !legacy_present {
            return Ok(None);
        }
        let row = self
            .connection
            .query_row(
                "SELECT typeof(operation_id), typeof(sequence), sequence,
                        typeof(record), length(record),
                        CASE WHEN typeof(record) = 'blob'
                                  AND length(record) BETWEEN 0 AND ?2
                             THEN record ELSE NULL END
                 FROM operation_journal WHERE operation_id = ?1",
                params![
                    operation_id.to_string(),
                    if self.recording_prestart_scope.get() {
                        recording_prestart_budget::MAX_RECORD_BYTES
                    } else {
                        MAX_RECORD_BYTES as i64
                    }
                ],
                |row| {
                    let operation_id_type: String = row.get(0)?;
                    let sequence_type: String = row.get(1)?;
                    let sequence = match row.get_ref(2)? {
                        ValueRef::Integer(value) => Some(value),
                        _ => None,
                    };
                    let record_type: String = row.get(3)?;
                    let record_length = match row.get_ref(4)? {
                        ValueRef::Integer(value) => Some(value),
                        ValueRef::Null => None,
                        _ => None,
                    };
                    let encoded = match row.get_ref(5)? {
                        ValueRef::Blob(value) => {
                            #[cfg(test)]
                            POINT_ROW_MATERIALIZED_BYTES.with(|bytes| bytes.set(value.len()));
                            Some(value.to_vec())
                        }
                        _ => None,
                    };
                    Ok((
                        operation_id_type,
                        sequence_type,
                        sequence,
                        record_type,
                        record_length,
                        encoded,
                    ))
                },
            )
            .optional()?;
        let Some((operation_id_type, sequence_type, sequence, record_type, record_length, encoded)) =
            row
        else {
            return Ok(None);
        };
        if operation_id_type != "text" {
            return Err(retained_error(
                Some(operation_id),
                String::from("operation id column must contain a SQLite TEXT key"),
            ));
        }
        if sequence_type != "integer" {
            return Err(retained_error(
                Some(operation_id),
                String::from("sequence column must contain a SQLite integer"),
            ));
        }
        let Some(sequence) = sequence else {
            return Err(retained_error(
                Some(operation_id),
                String::from("sequence column is not an SQLite integer"),
            ));
        };
        if sequence <= 0 {
            return Err(retained_error(
                Some(operation_id),
                String::from("stored sequence is not positive"),
            ));
        }
        self.ensure_sequence_unique(sequence)?;
        if record_type != "blob" {
            return Err(retained_error(
                Some(operation_id),
                String::from("record column must contain a SQLite BLOB"),
            ));
        }
        let Some(record_length) = record_length else {
            return Err(retained_error(
                Some(operation_id),
                String::from("stored record length is NULL"),
            ));
        };
        if !(0..=MAX_RECORD_BYTES as i64).contains(&record_length) {
            return Err(retained_error(
                Some(operation_id),
                format!("stored record length {record_length} exceeds the bounded record size"),
            ));
        }
        let Some(encoded) = encoded else {
            return Err(retained_error(
                Some(operation_id),
                String::from("record column could not be read as a SQLite BLOB"),
            ));
        };
        if encoded.len() != record_length as usize || encoded.len() > MAX_RECORD_BYTES {
            return Err(retained_error(
                Some(operation_id),
                String::from("stored record length changed or exceeded the bound"),
            ));
        }
        let record = if self.recording_prestart_scope.get() {
            super::codec::decode_prestart_recording(
                &encoded,
                recording_prestart_budget::JSON_LIMITS,
            )
            .map_err(|error| match error {
                super::codec::BoundedDecodeError::LimitExceeded { .. } => {
                    self.recording_prestart_resource_refused.set(true);
                    OperationJournalStoreError::RecordingPrestartResourceBound
                }
                super::codec::BoundedDecodeError::Decode(error) => {
                    retained_decode_error(operation_id, error)
                }
            })?
            .ok_or_else(|| {
                self.recording_prestart_resource_refused.set(true);
                OperationJournalStoreError::RecordingPrestartResourceBound
            })?
        } else {
            decode(&encoded).map_err(|error| retained_decode_error(operation_id, error))?
        };
        if record.operation_id != operation_id {
            return Err(retained_error(
                Some(operation_id),
                String::from("record identity does not match its SQLite key"),
            ));
        }
        match classify_record(&record) {
            RecordConsistency::Consistent => Ok(Some(JournalRow {
                sequence,
                encoded,
                record,
            })),
            RecordConsistency::LegacyMissingPublicationEvidence => Err(retained_error(
                Some(operation_id),
                String::from("legacy record is retained without publication evidence"),
            )),
            RecordConsistency::Invalid(reason) => Err(retained_error(Some(operation_id), reason)),
        }
    }

    fn rebuild_ordinary_claim_snapshot_inside_transaction(
        &self,
    ) -> Result<OrdinaryClaimRebuildSnapshot, OperationJournalStoreError> {
        self.rebuild_ordinary_claim_snapshot_with_funding(None, &mut None)
    }

    fn rebuild_ordinary_claim_snapshot_with_funding(
        &self,
        input: Option<&super::owner::InputPermit>,
        collection: &mut Option<admission_rebuild_backing::Collection>,
    ) -> Result<OrdinaryClaimRebuildSnapshot, OperationJournalStoreError> {
        self.validate_namespace_for_precommit()?;
        if let Some(input) = input {
            *collection = Some(admission_rebuild_backing::Collection::new(
                &self.connection,
                input,
            )?);
        }
        let mut claim_rows = collection
            .as_ref()
            .map_or_else(Vec::new, |backing| Vec::with_capacity(backing.claims));
        let mut claim_cursor = None;
        let mut diagnostics = Vec::new();
        let mut fatal = false;
        loop {
            let page_result = match input {
                Some(input) => {
                    ordinary_claims::read_funded_page(&self.connection, input, claim_cursor)
                }
                None => ordinary_claims::read_page(
                    &self.connection,
                    claim_cursor,
                    ordinary_claims::MAX_PAGE_ROWS,
                )
                .map_err(ordinary_claims::PageFailure::original),
            };
            let page = match page_result {
                Ok(page) => page,
                Err(error) => {
                    if error.is_refusal() {
                        return Err(error.into_refusal());
                    }
                    push_rebuild_diagnostic(
                        &mut diagnostics,
                        format!(
                            "ordinary-claim page was retained but could not be decoded: {error}"
                        ),
                    );
                    fatal = true;
                    break;
                }
            };
            claim_rows.extend(page.records().iter().cloned());
            let Some(next_cursor) = page.next_cursor() else {
                break;
            };
            claim_cursor = Some(next_cursor);
        }

        let mut release_rows = BTreeMap::new();
        let mut release_id_owners = BTreeMap::new();
        let mut conflicting_release_claims = BTreeSet::new();
        let mut release_cursor = None;
        if !fatal {
            loop {
                let page_result = match input {
                    Some(input) => ordinary_claims::read_funded_release_page(
                        &self.connection,
                        input,
                        release_cursor,
                    ),
                    None => ordinary_claims::read_release_page(
                        &self.connection,
                        release_cursor,
                        ordinary_claims::MAX_PAGE_ROWS,
                    )
                    .map_err(ordinary_claims::PageFailure::original),
                };
                let page = match page_result {
                    Ok(page) => page,
                    Err(error) => {
                        if error.is_refusal() {
                            return Err(error.into_refusal());
                        }
                        push_rebuild_diagnostic(
                            &mut diagnostics,
                            format!(
                                "ordinary-claim release page was retained but could not be decoded: {error}"
                            ),
                        );
                        fatal = true;
                        break;
                    }
                };
                for release in page.records() {
                    if let Some(previous) = release_rows.insert(release.claim_id(), *release) {
                        conflicting_release_claims.insert(previous.claim_id());
                        conflicting_release_claims.insert(release.claim_id());
                    }
                    if let Some(previous_claim) =
                        release_id_owners.insert(release.release_id(), release.claim_id())
                    {
                        conflicting_release_claims.insert(previous_claim);
                        conflicting_release_claims.insert(release.claim_id());
                    }
                }
                let Some(next_cursor) = page.next_cursor() else {
                    break;
                };
                release_cursor = Some(next_cursor);
            }
        }

        // Incremental insertion avoids FromIterator's additional sorting Vec.
        let mut claim_ids = BTreeSet::new();
        for claim in &claim_rows {
            claim_ids.insert(claim.claim_id());
        }
        let mut rebuild_claims = Vec::with_capacity(claim_rows.len());
        for claim in claim_rows {
            let observed = match input {
                Some(input) => self.observe_claim_operation(claim.operation_id(), input),
                None => self.read_row(claim.operation_id()).map(|row| row.is_some()),
            };
            // Pool refusal cannot become an audit-only snapshot followed by admission.
            if matches!(
                observed,
                Err(OperationJournalStoreError::HistoricalClaimResourceBound)
            ) {
                return Err(OperationJournalStoreError::HistoricalClaimResourceBound);
            }
            let operation_valid = match observed {
                Ok(true) => true,
                Ok(false) => {
                    push_rebuild_diagnostic(
                        &mut diagnostics,
                        format!(
                            "ordinary claim {} references missing legacy operation {}",
                            claim.claim_id(),
                            claim.operation_id()
                        ),
                    );
                    false
                }
                Err(error) => {
                    push_rebuild_diagnostic(
                        &mut diagnostics,
                        format!(
                            "ordinary claim {} operation {} requires audit: {error}",
                            claim.claim_id(),
                            claim.operation_id()
                        ),
                    );
                    false
                }
            };
            // The original observer error is now dropped; its bounded replacement
            // diagnostic has a separately precharged retained graph/reply debit.
            if let Some(input) = input {
                input.historical_error_consumed();
            }
            let mut audit_pending = !operation_valid;
            let mut release_id = None;
            if let Some(release) = release_rows.remove(&claim.claim_id()) {
                if conflicting_release_claims.contains(&claim.claim_id()) {
                    push_rebuild_diagnostic(
                        &mut diagnostics,
                        format!(
                            "ordinary claim {} has conflicting durable release identities",
                            claim.claim_id()
                        ),
                    );
                    audit_pending = true;
                } else if release.operation_id() != claim.operation_id() {
                    push_rebuild_diagnostic(
                        &mut diagnostics,
                        format!(
                            "ordinary claim {} release operation {} mismatches claim operation {}",
                            claim.claim_id(),
                            release.operation_id(),
                            claim.operation_id()
                        ),
                    );
                    audit_pending = true;
                } else if operation_valid {
                    release_id = Some(release.release_id());
                }
            }
            rebuild_claims.push(RebuildClaim {
                operation_id: claim.operation_id(),
                claim_id: claim.claim_id(),
                volume: claim.volume().clone(),
                allocation_class: claim.allocation_class(),
                allocated_bytes: claim.allocated_bytes(),
                release_id,
                audit_pending,
            });
        }

        for (claim_id, release) in release_rows {
            if !claim_ids.contains(&claim_id) {
                push_rebuild_diagnostic(
                    &mut diagnostics,
                    format!(
                        "orphan ordinary-claim release {} has no durable claim row",
                        release.claim_id()
                    ),
                );
            }
        }

        if fatal {
            return Ok(OrdinaryClaimRebuildSnapshot {
                claims: rebuild_claims,
                readiness: CapacityRebuildReadiness::reconciliation_required(diagnostics),
                durable_zero: false,
            });
        }
        let has_outstanding = rebuild_claims
            .iter()
            .any(|claim| claim.release_id().is_none());
        if diagnostics.is_empty() && has_outstanding {
            return Ok(OrdinaryClaimRebuildSnapshot {
                claims: rebuild_claims,
                readiness: CapacityRebuildReadiness::ready(),
                durable_zero: false,
            });
        }
        let durable_zero = diagnostics.is_empty()
            && !has_outstanding
            && rebuild_claims
                .iter()
                .all(|claim| claim.release_id().is_some() && !claim.audit_pending());
        if diagnostics.is_empty() {
            push_rebuild_diagnostic(
                &mut diagnostics,
                String::from("ordinary-claim rebuild found no outstanding retained claims"),
            );
        }
        Ok(OrdinaryClaimRebuildSnapshot {
            claims: rebuild_claims,
            readiness: CapacityRebuildReadiness::recovery_only(diagnostics),
            durable_zero,
        })
    }

    fn validate_chunk_claims_inside_transaction(&self) -> Result<(), OperationJournalStoreError> {
        self.validate_recording_inventory_inside_transaction()?;
        self.validate_database_claims_inside_transaction()?;
        self.validate_database_attempts_inside_transaction()?;
        if chunk_claims::exceeds_row_limit(&self.connection, ordinary_claims::MAX_PAGE_ROWS)
            .map_err(|error| OperationJournalStoreError::ChunkClaim {
                reason: error.to_string(),
            })?
        {
            return Err(OperationJournalStoreError::ChunkClaim {
                reason: format!(
                    "chunk-claim relation exceeds the bounded {} row audit",
                    ordinary_claims::MAX_PAGE_ROWS
                ),
            });
        }
        let mut cursor = None;
        loop {
            let page =
                chunk_claims::read_page(&self.connection, cursor, chunk_claims::MAX_PAGE_ROWS)
                    .map_err(|error| OperationJournalStoreError::ChunkClaim {
                        reason: error.to_string(),
                    })?;
            for chunk in page.records() {
                let Some(claim) = ordinary_claims::read_claim(&self.connection, chunk.claim_id())
                    .map_err(|error| OperationJournalStoreError::ChunkClaim {
                    reason: error.to_string(),
                })?
                else {
                    return Err(OperationJournalStoreError::ChunkClaim {
                        reason: format!("chunk {} has no ordinary claim row", chunk.chunk_id()),
                    });
                };
                if claim.operation_id() != chunk.operation_id()
                    || claim.volume() != chunk.volume()
                    || claim.allocation_class() != chunk.allocation_class()
                    || claim.allocated_bytes() != chunk.allocated_bytes()
                {
                    return Err(OperationJournalStoreError::ChunkClaim {
                        reason: format!(
                            "chunk {} disagrees with its ordinary claim row",
                            chunk.chunk_id()
                        ),
                    });
                }
                let Some(operation) = self.read_row(chunk.operation_id())? else {
                    return Err(OperationJournalStoreError::ChunkClaim {
                        reason: format!(
                            "chunk {} references missing operation {}",
                            chunk.chunk_id(),
                            chunk.operation_id()
                        ),
                    });
                };
                let participant_present =
                    operation
                        .record
                        .universal_intent
                        .as_ref()
                        .is_some_and(|intent| {
                            intent
                                .participants()
                                .descriptors()
                                .iter()
                                .any(|participant| participant.id() == chunk.participant_id())
                        });
                if !participant_present {
                    return Err(OperationJournalStoreError::ChunkClaim {
                        reason: format!(
                            "chunk {} references a participant outside operation {}",
                            chunk.chunk_id(),
                            chunk.operation_id()
                        ),
                    });
                }
                let release = ordinary_claims::read_release(&self.connection, chunk.claim_id())
                    .map_err(|error| OperationJournalStoreError::ChunkClaim {
                        reason: error.to_string(),
                    })?;
                match chunk.state() {
                    chunk_claims::ChunkClaimState::Released => {
                        if release.as_ref().is_none_or(|release| {
                            release.operation_id() != chunk.operation_id()
                                || release.release_id() != chunk.release_id().unwrap_or_default()
                        }) {
                            return Err(OperationJournalStoreError::ChunkClaim {
                                reason: format!(
                                    "released chunk {} lacks matching ordinary release evidence",
                                    chunk.chunk_id()
                                ),
                            });
                        }
                    }
                    chunk_claims::ChunkClaimState::Claimed
                    | chunk_claims::ChunkClaimState::Issued => {
                        if release.is_some() {
                            return Err(OperationJournalStoreError::ChunkClaim {
                                reason: format!(
                                    "unreleased chunk {} has ordinary release evidence",
                                    chunk.chunk_id()
                                ),
                            });
                        }
                    }
                }
            }
            let Some(next) = page.next_cursor() else {
                break;
            };
            cursor = Some(next);
        }
        Ok(())
    }

    fn validate_namespace(&self) -> Result<(), OperationJournalStoreError> {
        if !self.connection.is_autocommit() {
            return Err(audit_error(
                &self.database_path,
                "operation journal connection has an active transaction",
            ));
        }
        self.validate_namespace_for_precommit()
    }

    /// Validate the store identity and sidecar namespace at a transaction pre-commit boundary.
    ///
    /// This is deliberately separate from [`Self::validate_namespace`], whose autocommit check
    /// rejects an active transaction. DELETE mode may retain its ordinary rollback journal while
    /// this exclusive connection remains open; WAL/SHM and unsafe/non-regular entries are still
    /// rejected here before a full-progress mutation is committed.
    fn validate_namespace_for_precommit(&self) -> Result<(), OperationJournalStoreError> {
        self.validate_namespace_without_sidecars()?;
        reject_unexpected_sidecars(&self.database_root, &self.database_path, true)
    }

    fn validate_namespace_without_sidecars(&self) -> Result<(), OperationJournalStoreError> {
        self.database_root
            .validate_profile_guard(&self.profile_guard)?;
        validate_database_file_identity(
            &self.database_root,
            &self.database_path,
            &self.database_identity,
        )?;
        validate_connection_identity(
            &self.connection,
            &self.database_root,
            &self.database_path,
            &self.database_identity,
        )
    }

    fn operation_family_presence(
        &self,
        operation_id: OperationId,
    ) -> Result<(bool, bool), OperationJournalStoreError> {
        let key = operation_id.to_string();
        let legacy_present: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM operation_journal WHERE operation_id = ?1)",
            params![key.as_str()],
            |row| row.get(0),
        )?;
        let full_progress_present: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM operation_journal_full_progress WHERE operation_id = ?1)",
            params![key.as_str()],
            |row| row.get(0),
        )?;
        Ok((legacy_present, full_progress_present))
    }

    fn ensure_sequence_unique(&self, sequence: i64) -> Result<(), OperationJournalStoreError> {
        let count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM (
                 SELECT sequence FROM operation_journal
                 WHERE typeof(sequence) = 'integer' AND sequence = ?1
                 UNION ALL
                 SELECT sequence FROM operation_journal_full_progress
                 WHERE typeof(sequence) = 'integer' AND sequence = ?1
                 UNION ALL
                 SELECT sequence FROM operation_journal_staging_checkpoints
                 WHERE typeof(sequence) = 'integer' AND sequence = ?1
                 UNION ALL
                 SELECT sequence FROM operation_journal_chunk_staging
                 WHERE typeof(sequence) = 'integer' AND sequence = ?1
                 UNION ALL
                 SELECT sequence FROM operation_journal_recording_chunks
                 WHERE typeof(sequence) = 'integer' AND sequence = ?1
             )",
            params![sequence],
            |row| row.get(0),
        )?;
        if count > 1 {
            return Err(audit_error(
                &self.database_path,
                format!("duplicate journal-observation sequence {sequence}"),
            ));
        }
        Ok(())
    }
}

fn push_rebuild_diagnostic(diagnostics: &mut Vec<String>, reason: String) {
    if diagnostics.len() >= MAX_REBUILD_DIAGNOSTICS {
        return;
    }
    let bounded: String = reason.chars().take(MAX_REBUILD_DIAGNOSTIC_CHARS).collect();
    diagnostics.push(bounded);
}

#[allow(dead_code)]
fn release_ordinary_claim_transition_inside_transaction(
    connection: &Connection,
    request: OrdinaryClaimReleaseRequest,
) -> Result<OrdinaryClaimReleaseTransition, OrdinaryClaimReleaseError> {
    let claim = ordinary_claims::read_claim(connection, request.claim_id())
        .map_err(|error| OrdinaryClaimReleaseError::Persistence {
            source: ordinary_claim_read_error(error),
        })?
        .ok_or(OrdinaryClaimReleaseError::ClaimNotFound {
            claim_id: request.claim_id(),
        })?;
    if claim.operation_id() != request.operation_id() {
        return Err(OrdinaryClaimReleaseError::OperationMismatch {
            claim_id: request.claim_id(),
        });
    }
    // Post-intent chunk claims have a stronger lifecycle than a generic ordinary release.  A
    // direct ordinary transition must not decrement their provider charge while leaving the
    // chunk relation Issued; only the atomic chunk transition may create its release evidence.
    let requested_release_id = request.release_id().to_string();
    if let Some((chunk_state, chunk_release_id)) = connection
        .query_row(
            &format!(
                "SELECT state, release_id FROM {} WHERE claim_id = ?1",
                chunk_claims::TABLE_NAME
            ),
            params![request.claim_id().to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()
        .map_err(|error| OrdinaryClaimReleaseError::Persistence {
            source: OperationJournalStoreError::Sql(error),
        })?
    {
        if chunk_state != "Released"
            || chunk_release_id.as_deref() != Some(requested_release_id.as_str())
        {
            return Err(OrdinaryClaimReleaseError::ChunkClaimRequiresOwner {
                claim_id: request.claim_id(),
            });
        }
    }
    let durable = ordinary_claims::OrdinaryClaimReleaseRecord::new(
        request.claim_id(),
        request.operation_id(),
        request.release_id(),
    );
    if let Some(existing) =
        ordinary_claims::read_release(connection, request.claim_id()).map_err(|error| {
            OrdinaryClaimReleaseError::Persistence {
                source: ordinary_claim_read_error(error),
            }
        })?
    {
        if existing == durable {
            return Ok(OrdinaryClaimReleaseTransition::Replay(
                OrdinaryClaimReleaseReceipt::from_claim(&claim, request.release_id()),
            ));
        }
        return Err(OrdinaryClaimReleaseError::ReleaseConflict {
            claim_id: request.claim_id(),
        });
    }
    ordinary_claims::insert_release_inside_transaction(connection, durable)
        .and_then(|()| ordinary_claims::verify_release_inside_transaction(connection, durable))
        .map_err(|error| OrdinaryClaimReleaseError::Persistence {
            source: ordinary_claim_write_error(error),
        })?;
    Ok(OrdinaryClaimReleaseTransition::Inserted(
        OrdinaryClaimReleaseReceipt::from_claim(&claim, request.release_id()),
    ))
}

fn ordinary_claim_write_error(
    error: ordinary_claims::OrdinaryClaimWriteError,
) -> OperationJournalStoreError {
    OperationJournalStoreError::InconsistentRecord {
        reason: format!("ordinary claim settlement failed: {error}"),
    }
}

#[allow(dead_code)]
fn ordinary_claim_read_error(
    error: ordinary_claims::OrdinaryClaimReadError,
) -> OperationJournalStoreError {
    OperationJournalStoreError::InconsistentRecord {
        reason: format!("ordinary claim release read failed: {error}"),
    }
}

fn failed_insert(
    operation_id: OperationId,
    stage: JournalInsertWriteStage,
    source: OperationJournalStoreError,
    attempted_sequence: Option<i64>,
    encoded_bytes: usize,
    rollback_error: Option<Box<OperationJournalStoreError>>,
) -> JournalInsertOutcome {
    JournalInsertOutcome::Failed(JournalInsertFailure {
        operation_id,
        stage,
        source,
        attempted_sequence,
        encoded_bytes,
        rollback_error,
    })
}

fn validate_intent_record(
    record: &StoredOperationRecord,
) -> Result<(), OperationJournalStoreError> {
    if record.phase != OperationPhase::IntentDurable {
        return Err(OperationJournalStoreError::NotIntentDurable {
            phase: record.phase,
        });
    }
    if record.capacity_plan.is_none() && record.full_capacity_plan.is_none() {
        return Err(OperationJournalStoreError::MissingCapacityPlan);
    }
    if record.capacity_plan.is_some() && record.full_capacity_plan.is_some() {
        return Err(OperationJournalStoreError::InconsistentRecord {
            reason: String::from("durable intent cannot carry both legacy and full capacity plans"),
        });
    }
    if record.disposition != OperationDisposition::None {
        return Err(OperationJournalStoreError::InconsistentRecord {
            reason: String::from("durable intent must have the None disposition"),
        });
    }
    for (present, kind) in [
        (record.prepared.is_some(), "prepared"),
        (record.staged.is_some(), "staged"),
        (record.published().is_some(), "published"),
        (
            record.absent_final_recovery_observation.is_some(),
            "absent-final recovery observation",
        ),
        (
            record.absent_final_transaction_owned_proof.is_some(),
            "absent-final transaction-owned proof",
        ),
        (
            record.absent_final_adoption_evidence.is_some(),
            "absent-final adoption evidence",
        ),
        (
            record.replacement_qualification.is_some(),
            "replacement qualification",
        ),
    ] {
        if present {
            return Err(OperationJournalStoreError::IntentCarriesExecutionEvidence { kind });
        }
    }
    match classify_record(record) {
        RecordConsistency::Consistent => Ok(()),
        RecordConsistency::LegacyMissingPublicationEvidence => {
            Err(OperationJournalStoreError::InconsistentRecord {
                reason: String::from("legacy publication evidence is not an admissible intent"),
            })
        }
        RecordConsistency::Invalid(reason) => {
            Err(OperationJournalStoreError::InconsistentRecord { reason })
        }
    }
}

fn full_capacity_plan_for_record(
    record: &StoredOperationRecord,
) -> Result<FullCapacityPlan, OperationJournalStoreError> {
    match (
        record.capacity_plan.as_ref(),
        record.full_capacity_plan.as_ref(),
    ) {
        (Some(_), Some(_)) => Err(OperationJournalStoreError::InconsistentRecord {
            reason: String::from("durable intent cannot carry both legacy and full capacity plans"),
        }),
        (Some(legacy), None) => FullCapacityPlan::from_legacy(legacy).map_err(|error| {
            OperationJournalStoreError::InconsistentRecord {
                reason: format!("legacy capacity plan is invalid: {error}"),
            }
        }),
        (None, Some(full)) => {
            full.validate()
                .map_err(|error| OperationJournalStoreError::InconsistentRecord {
                    reason: format!("full capacity plan is invalid: {error}"),
                })?;
            Ok(full.clone())
        }
        (None, None) => Err(OperationJournalStoreError::MissingCapacityPlan),
    }
}

fn open_database_file(
    root: &BoundDatabaseRoot,
    name: &Path,
    path: &Path,
) -> Result<(File, bool), OperationJournalStoreError> {
    sqlite_identity::open_database_handle(root, name, path).map_err(map_identity_error)
}

fn ensure_regular_file(file: &File, path: &Path) -> Result<(), OperationJournalStoreError> {
    sqlite_identity::ensure_regular_file(file, path).map_err(map_identity_error)
}

struct PreexistingRollbackJournal {
    _file: File,
    identity: String,
}

fn validate_pre_sqlite_sidecars(
    root: &BoundDatabaseRoot,
    database_path: &Path,
    expected: Option<&PreexistingRollbackJournal>,
) -> Result<(), OperationJournalStoreError> {
    let observed = inspect_preexisting_sidecars(root, database_path)?;
    match (expected, observed) {
        (None, None) => Ok(()),
        (Some(expected), Some(observed)) if expected.identity == observed.identity => Ok(()),
        (Some(_), None) => Err(audit_error(
            database_path,
            "validated SQLite rollback sidecar disappeared before database open",
        )),
        (None, Some(_)) => Err(audit_error(
            database_path,
            "SQLite rollback sidecar appeared before database open",
        )),
        (Some(_), Some(_)) => Err(audit_error(
            database_path,
            "validated SQLite rollback sidecar was replaced before database open",
        )),
    }
}

fn inspect_preexisting_sidecars(
    root: &BoundDatabaseRoot,
    database_path: &Path,
) -> Result<Option<PreexistingRollbackJournal>, OperationJournalStoreError> {
    let mut rollback_journal = None;
    for suffix in [SQLITE_JOURNAL_SUFFIX, SQLITE_WAL_SUFFIX, SQLITE_SHM_SUFFIX] {
        let name = format!("{JOURNAL_DATABASE_FILE_NAME}{suffix}");
        match root.symlink_metadata(Path::new(&name)) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(audit_error(
                        database_path,
                        format!("SQLite sidecar {name} is a symlink"),
                    ));
                }
                if !metadata.is_file() {
                    return Err(audit_error(
                        database_path,
                        format!("SQLite sidecar {name} is not a regular file"),
                    ));
                }
                if suffix != SQLITE_JOURNAL_SUFFIX {
                    return Err(audit_error(
                        database_path,
                        format!("unexpected SQLite sidecar {name} is present"),
                    ));
                }
                let mut file = root
                    .open_file_nofollow(Path::new(&name), false)
                    .map_err(|source| io_error(database_path, source))?;
                validate_inactive_rollback_journal(&mut file, database_path, &name)?;
                let identity =
                    stable_filesystem_identity_from_open_file(&file).ok_or_else(|| {
                        OperationJournalStoreError::RequiresAudit {
                            path: database_path.to_path_buf(),
                            reason: format!(
                                "stable identity is unavailable for SQLite sidecar {name}"
                            ),
                        }
                    })?;
                rollback_journal = Some(PreexistingRollbackJournal {
                    _file: file,
                    identity,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error(database_path, source)),
        }
    }
    Ok(rollback_journal)
}

fn validate_inactive_rollback_journal(
    file: &mut File,
    database_path: &Path,
    sidecar_name: &str,
) -> Result<(), OperationJournalStoreError> {
    let metadata = file
        .metadata()
        .map_err(|source| io_error(database_path, source))?;
    if !metadata.is_file() {
        return Err(audit_error(
            database_path,
            format!("SQLite sidecar {sidecar_name} is not a regular file"),
        ));
    }
    let length = metadata.len();
    if length == 0 {
        return Ok(());
    }
    if length < SQLITE_ROLLBACK_HEADER_BYTES as u64 {
        return Err(audit_error(
            database_path,
            format!("SQLite rollback sidecar {sidecar_name} is an ambiguous {length}-byte file"),
        ));
    }
    let mut header = [0_u8; SQLITE_ROLLBACK_HEADER_BYTES];
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.read_exact(&mut header))
        .map_err(|source| io_error(database_path, source))?;
    if header.iter().any(|byte| *byte != 0) {
        return Err(audit_error(
            database_path,
            format!("SQLite rollback sidecar {sidecar_name} has an active or ambiguous header"),
        ));
    }
    Ok(())
}

fn validate_rollback_journal_identity(
    root: &BoundDatabaseRoot,
    database_path: &Path,
    expected_identity: &str,
) -> Result<(), OperationJournalStoreError> {
    let sidecar_name = format!("{JOURNAL_DATABASE_FILE_NAME}{SQLITE_JOURNAL_SUFFIX}");
    let metadata = match root.symlink_metadata(Path::new(&sidecar_name)) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(io_error(database_path, source)),
    };
    if metadata.file_type().is_symlink() {
        return Err(audit_error(
            database_path,
            format!("SQLite sidecar {sidecar_name} is a symlink"),
        ));
    }
    if !metadata.is_file() {
        return Err(audit_error(
            database_path,
            format!("SQLite sidecar {sidecar_name} is not a regular file"),
        ));
    }
    let file = root
        .open_file_nofollow(Path::new(&sidecar_name), false)
        .map_err(|source| io_error(database_path, source))?;
    let mut file = file;
    validate_inactive_rollback_journal(&mut file, database_path, &sidecar_name)?;
    let actual_identity = stable_filesystem_identity_from_open_file(&file).ok_or_else(|| {
        OperationJournalStoreError::RequiresAudit {
            path: database_path.to_path_buf(),
            reason: format!("stable identity is unavailable for SQLite sidecar {sidecar_name}"),
        }
    })?;
    if actual_identity != expected_identity {
        return Err(audit_error(
            database_path,
            format!("SQLite sidecar {sidecar_name} was replaced while opening the database"),
        ));
    }
    Ok(())
}

fn reject_unexpected_sidecars(
    root: &BoundDatabaseRoot,
    database_path: &Path,
    allow_rollback_journal: bool,
) -> Result<(), OperationJournalStoreError> {
    for suffix in [SQLITE_JOURNAL_SUFFIX, SQLITE_WAL_SUFFIX, SQLITE_SHM_SUFFIX] {
        let name = format!("{JOURNAL_DATABASE_FILE_NAME}{suffix}");
        match root.symlink_metadata(Path::new(&name)) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(audit_error(
                        database_path,
                        format!("SQLite sidecar {name} is a symlink"),
                    ));
                }
                if !metadata.is_file() {
                    return Err(audit_error(
                        database_path,
                        format!("SQLite sidecar {name} is not a regular file"),
                    ));
                }
                if suffix == SQLITE_JOURNAL_SUFFIX && allow_rollback_journal {
                    continue;
                }
                return Err(audit_error(
                    database_path,
                    format!("unexpected SQLite sidecar {name} is present"),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error(database_path, source)),
        }
    }
    Ok(())
}

fn validate_sqlite_header(file: &mut File, path: &Path) -> Result<u32, OperationJournalStoreError> {
    let length = file
        .metadata()
        .map_err(|source| io_error(path, source))?
        .len();
    if length < SQLITE_HEADER_BYTES as u64 {
        return Err(audit_error(
            path,
            "existing SQLite file is shorter than its header",
        ));
    }
    let mut header = [0_u8; SQLITE_HEADER_BYTES];
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.read_exact(&mut header))
        .map_err(|source| io_error(path, source))?;
    if &header[..SQLITE_HEADER_MAGIC.len()] != SQLITE_HEADER_MAGIC {
        return Err(audit_error(path, "existing file is not SQLite format 3"));
    }
    if header[18] != 1 || header[19] != 1 {
        return Err(audit_error(
            path,
            format!(
                "existing SQLite file uses unsupported format versions write={} read={}",
                header[18], header[19]
            ),
        ));
    }
    let application_id = u32::from_be_bytes(
        header[SQLITE_APPLICATION_ID_OFFSET..SQLITE_APPLICATION_ID_OFFSET + 4]
            .try_into()
            .expect("SQLite application-id field has fixed width"),
    );
    let user_version = u32::from_be_bytes(
        header[SQLITE_USER_VERSION_OFFSET..SQLITE_USER_VERSION_OFFSET + 4]
            .try_into()
            .expect("SQLite user-version field has fixed width"),
    );
    if application_id != JOURNAL_APPLICATION_ID {
        return Err(audit_error(
            path,
            format!("SQLite application id {application_id:#x} is not the operation-journal id"),
        ));
    }
    if !matches!(
        user_version,
        LEGACY_JOURNAL_SCHEMA_VERSION
            | PREVIOUS_JOURNAL_SCHEMA_VERSION
            | RECOVERY_RESERVE_SCHEMA_VERSION
            | RECOVERY_RESERVE_BINDING_SCHEMA_VERSION
            | ORDINARY_CLAIM_SCHEMA_VERSION
            | ORDINARY_CLAIM_RELEASE_SCHEMA_VERSION
            | CHUNK_CLAIM_SCHEMA_VERSION
            | STAGING_CHECKPOINT_SCHEMA_VERSION
            | CHUNK_STAGING_SCHEMA_VERSION
            | RECORDING_CHUNKS_SCHEMA_VERSION
            | DATABASE_CLAIMS_SCHEMA_VERSION
            | DATABASE_ATTEMPTS_SCHEMA_VERSION
            | JOURNAL_SCHEMA_VERSION
    ) {
        return Err(audit_error(
            path,
            format!(
                "SQLite user version {user_version} is not a supported operation-journal schema"
            ),
        ));
    }
    Ok(user_version)
}

fn read_sqlite_user_version(
    connection: &Connection,
    path: &Path,
) -> Result<u32, OperationJournalStoreError> {
    let user_version: i64 =
        connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let user_version = u32::try_from(user_version).map_err(|_| {
        audit_error(
            path,
            format!("SQLite user version {user_version} is outside the supported range"),
        )
    })?;
    if !matches!(
        user_version,
        LEGACY_JOURNAL_SCHEMA_VERSION
            | PREVIOUS_JOURNAL_SCHEMA_VERSION
            | RECOVERY_RESERVE_SCHEMA_VERSION
            | RECOVERY_RESERVE_BINDING_SCHEMA_VERSION
            | ORDINARY_CLAIM_SCHEMA_VERSION
            | ORDINARY_CLAIM_RELEASE_SCHEMA_VERSION
            | CHUNK_CLAIM_SCHEMA_VERSION
            | STAGING_CHECKPOINT_SCHEMA_VERSION
            | CHUNK_STAGING_SCHEMA_VERSION
            | RECORDING_CHUNKS_SCHEMA_VERSION
            | DATABASE_CLAIMS_SCHEMA_VERSION
            | DATABASE_ATTEMPTS_SCHEMA_VERSION
            | JOURNAL_SCHEMA_VERSION
    ) {
        return Err(audit_error(
            path,
            format!(
                "SQLite user version {user_version} is not a supported operation-journal schema"
            ),
        ));
    }
    Ok(user_version)
}

fn validate_existing_schema(
    connection: &Connection,
    path: &Path,
    schema_version: u32,
) -> Result<(), OperationJournalStoreError> {
    let actual_version = read_sqlite_user_version(connection, path)?;
    if actual_version != schema_version {
        return Err(audit_error(
            path,
            format!(
                "SQLite user version {actual_version} does not match expected schema {schema_version}"
            ),
        ));
    }
    let objects: Vec<(String, String)> = connection
        .prepare(
            "SELECT name, type FROM sqlite_master
             WHERE name NOT LIKE 'sqlite_%' ORDER BY name LIMIT ?1",
        )?
        .query_map(params![MAX_SCHEMA_OBJECTS + 1], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let expected_objects = match schema_version {
        LEGACY_JOURNAL_SCHEMA_VERSION => {
            vec![(String::from(JOURNAL_TABLE_NAME), String::from("table"))]
        }
        PREVIOUS_JOURNAL_SCHEMA_VERSION => vec![
            (String::from(JOURNAL_TABLE_NAME), String::from("table")),
            (
                String::from(FULL_PROGRESS_TABLE_NAME),
                String::from("table"),
            ),
        ],
        RECOVERY_RESERVE_SCHEMA_VERSION => vec![
            (String::from(JOURNAL_TABLE_NAME), String::from("table")),
            (
                String::from(FULL_PROGRESS_TABLE_NAME),
                String::from("table"),
            ),
            (
                String::from(recovery_reserve::CHARGE_TABLE_NAME),
                String::from("table"),
            ),
            (
                String::from(recovery_reserve::TRANSITION_TABLE_NAME),
                String::from("table"),
            ),
        ],
        RECOVERY_RESERVE_BINDING_SCHEMA_VERSION => vec![
            (String::from(JOURNAL_TABLE_NAME), String::from("table")),
            (
                String::from(FULL_PROGRESS_TABLE_NAME),
                String::from("table"),
            ),
            (
                String::from(recovery_reserve::CHARGE_TABLE_NAME),
                String::from("table"),
            ),
            (
                String::from(recovery_reserve_binding::TABLE_NAME),
                String::from("table"),
            ),
            (
                String::from(recovery_reserve::TRANSITION_TABLE_NAME),
                String::from("table"),
            ),
        ],
        ORDINARY_CLAIM_SCHEMA_VERSION => vec![
            (String::from(JOURNAL_TABLE_NAME), String::from("table")),
            (
                String::from(FULL_PROGRESS_TABLE_NAME),
                String::from("table"),
            ),
            (
                String::from(ordinary_claims::TABLE_NAME),
                String::from("table"),
            ),
            (
                String::from(recovery_reserve::CHARGE_TABLE_NAME),
                String::from("table"),
            ),
            (
                String::from(recovery_reserve_binding::TABLE_NAME),
                String::from("table"),
            ),
            (
                String::from(recovery_reserve::TRANSITION_TABLE_NAME),
                String::from("table"),
            ),
        ],
        ORDINARY_CLAIM_RELEASE_SCHEMA_VERSION => vec![
            (String::from(JOURNAL_TABLE_NAME), String::from("table")),
            (
                String::from(FULL_PROGRESS_TABLE_NAME),
                String::from("table"),
            ),
            (
                String::from(ordinary_claims::TABLE_NAME),
                String::from("table"),
            ),
            (
                String::from(ordinary_claims::RELEASE_TABLE_NAME),
                String::from("table"),
            ),
            (
                String::from(recovery_reserve::CHARGE_TABLE_NAME),
                String::from("table"),
            ),
            (
                String::from(recovery_reserve_binding::TABLE_NAME),
                String::from("table"),
            ),
            (
                String::from(recovery_reserve::TRANSITION_TABLE_NAME),
                String::from("table"),
            ),
        ],
        CHUNK_CLAIM_SCHEMA_VERSION
        | STAGING_CHECKPOINT_SCHEMA_VERSION
        | CHUNK_STAGING_SCHEMA_VERSION
        | RECORDING_CHUNKS_SCHEMA_VERSION
        | DATABASE_CLAIMS_SCHEMA_VERSION
        | DATABASE_ATTEMPTS_SCHEMA_VERSION
        | JOURNAL_SCHEMA_VERSION => {
            let mut expected = vec![
                (String::from(JOURNAL_TABLE_NAME), String::from("table")),
                (
                    String::from(chunk_claims::TABLE_NAME),
                    String::from("table"),
                ),
                (
                    String::from(FULL_PROGRESS_TABLE_NAME),
                    String::from("table"),
                ),
                (
                    String::from(ordinary_claims::TABLE_NAME),
                    String::from("table"),
                ),
                (
                    String::from(ordinary_claims::RELEASE_TABLE_NAME),
                    String::from("table"),
                ),
                (
                    String::from(recovery_reserve::CHARGE_TABLE_NAME),
                    String::from("table"),
                ),
                (
                    String::from(recovery_reserve_binding::TABLE_NAME),
                    String::from("table"),
                ),
                (
                    String::from(recovery_reserve::TRANSITION_TABLE_NAME),
                    String::from("table"),
                ),
            ];
            if schema_version >= STAGING_CHECKPOINT_SCHEMA_VERSION {
                expected.insert(
                    8,
                    (
                        String::from(staging_checkpoints::TABLE_NAME),
                        String::from("table"),
                    ),
                );
            }
            if schema_version >= CHUNK_STAGING_SCHEMA_VERSION {
                expected.insert(
                    2,
                    (
                        String::from(chunk_staging::TABLE_NAME),
                        String::from("table"),
                    ),
                );
            }
            if schema_version >= RECORDING_CHUNKS_SCHEMA_VERSION {
                expected.push((
                    String::from(recording_chunks::TABLE_NAME),
                    String::from("table"),
                ));
            }
            if schema_version >= DATABASE_CLAIMS_SCHEMA_VERSION {
                expected.push((
                    String::from(database_claims::BINDING_TABLE),
                    String::from("table"),
                ));
                expected.push((
                    String::from(database_claims::SLICE_TABLE),
                    String::from("table"),
                ));
            }
            if schema_version >= DATABASE_ATTEMPTS_SCHEMA_VERSION {
                expected.push((
                    String::from(database_attempts::START_TABLE),
                    String::from("table"),
                ));
                expected.push((
                    String::from(database_attempts::OUTCOME_TABLE),
                    String::from("table"),
                ));
            }
            if schema_version == JOURNAL_SCHEMA_VERSION {
                expected.push((
                    String::from(publication_bindings::TABLE_NAME),
                    String::from("table"),
                ));
            }
            expected.sort_unstable();
            expected
        }
        _ => {
            return Err(audit_error(
                path,
                format!("unsupported schema version {schema_version}"),
            ));
        }
    };
    if objects != expected_objects {
        return Err(audit_error(
            path,
            "existing SQLite schema has unexpected objects",
        ));
    }

    validate_table_schema(connection, path, JOURNAL_TABLE_NAME, CREATE_JOURNAL_TABLE)?;
    if matches!(
        schema_version,
        PREVIOUS_JOURNAL_SCHEMA_VERSION
            | RECOVERY_RESERVE_SCHEMA_VERSION
            | RECOVERY_RESERVE_BINDING_SCHEMA_VERSION
            | ORDINARY_CLAIM_SCHEMA_VERSION
            | ORDINARY_CLAIM_RELEASE_SCHEMA_VERSION
            | CHUNK_CLAIM_SCHEMA_VERSION
            | STAGING_CHECKPOINT_SCHEMA_VERSION
            | CHUNK_STAGING_SCHEMA_VERSION
            | RECORDING_CHUNKS_SCHEMA_VERSION
            | DATABASE_CLAIMS_SCHEMA_VERSION
            | DATABASE_ATTEMPTS_SCHEMA_VERSION
            | JOURNAL_SCHEMA_VERSION
    ) {
        validate_table_schema(
            connection,
            path,
            FULL_PROGRESS_TABLE_NAME,
            CREATE_FULL_PROGRESS_TABLE,
        )?;
    }
    if matches!(
        schema_version,
        RECOVERY_RESERVE_SCHEMA_VERSION
            | RECOVERY_RESERVE_BINDING_SCHEMA_VERSION
            | ORDINARY_CLAIM_SCHEMA_VERSION
            | ORDINARY_CLAIM_RELEASE_SCHEMA_VERSION
            | CHUNK_CLAIM_SCHEMA_VERSION
            | STAGING_CHECKPOINT_SCHEMA_VERSION
            | CHUNK_STAGING_SCHEMA_VERSION
            | RECORDING_CHUNKS_SCHEMA_VERSION
            | DATABASE_CLAIMS_SCHEMA_VERSION
            | DATABASE_ATTEMPTS_SCHEMA_VERSION
            | JOURNAL_SCHEMA_VERSION
    ) {
        validate_table_schema(
            connection,
            path,
            recovery_reserve::CHARGE_TABLE_NAME,
            recovery_reserve::CREATE_CHARGE_TABLE,
        )?;
        validate_table_schema(
            connection,
            path,
            recovery_reserve::TRANSITION_TABLE_NAME,
            recovery_reserve::CREATE_TRANSITION_TABLE,
        )?;
    }
    if matches!(
        schema_version,
        RECOVERY_RESERVE_BINDING_SCHEMA_VERSION
            | ORDINARY_CLAIM_SCHEMA_VERSION
            | ORDINARY_CLAIM_RELEASE_SCHEMA_VERSION
            | CHUNK_CLAIM_SCHEMA_VERSION
            | STAGING_CHECKPOINT_SCHEMA_VERSION
            | CHUNK_STAGING_SCHEMA_VERSION
            | RECORDING_CHUNKS_SCHEMA_VERSION
            | DATABASE_CLAIMS_SCHEMA_VERSION
            | DATABASE_ATTEMPTS_SCHEMA_VERSION
            | JOURNAL_SCHEMA_VERSION
    ) {
        validate_table_schema(
            connection,
            path,
            recovery_reserve_binding::TABLE_NAME,
            recovery_reserve_binding::CREATE_TABLE,
        )?;
    }
    if matches!(
        schema_version,
        ORDINARY_CLAIM_SCHEMA_VERSION
            | ORDINARY_CLAIM_RELEASE_SCHEMA_VERSION
            | CHUNK_CLAIM_SCHEMA_VERSION
            | STAGING_CHECKPOINT_SCHEMA_VERSION
            | CHUNK_STAGING_SCHEMA_VERSION
            | RECORDING_CHUNKS_SCHEMA_VERSION
            | DATABASE_CLAIMS_SCHEMA_VERSION
            | DATABASE_ATTEMPTS_SCHEMA_VERSION
            | JOURNAL_SCHEMA_VERSION
    ) {
        validate_table_schema(
            connection,
            path,
            ordinary_claims::TABLE_NAME,
            ordinary_claims::CREATE_TABLE,
        )?;
    }
    if matches!(
        schema_version,
        ORDINARY_CLAIM_RELEASE_SCHEMA_VERSION
            | CHUNK_CLAIM_SCHEMA_VERSION
            | STAGING_CHECKPOINT_SCHEMA_VERSION
            | CHUNK_STAGING_SCHEMA_VERSION
            | RECORDING_CHUNKS_SCHEMA_VERSION
            | DATABASE_CLAIMS_SCHEMA_VERSION
            | DATABASE_ATTEMPTS_SCHEMA_VERSION
            | JOURNAL_SCHEMA_VERSION
    ) {
        validate_table_schema(
            connection,
            path,
            ordinary_claims::RELEASE_TABLE_NAME,
            ordinary_claims::CREATE_RELEASE_TABLE,
        )?;
    }
    if matches!(
        schema_version,
        CHUNK_CLAIM_SCHEMA_VERSION
            | STAGING_CHECKPOINT_SCHEMA_VERSION
            | CHUNK_STAGING_SCHEMA_VERSION
            | RECORDING_CHUNKS_SCHEMA_VERSION
            | DATABASE_CLAIMS_SCHEMA_VERSION
            | DATABASE_ATTEMPTS_SCHEMA_VERSION
            | JOURNAL_SCHEMA_VERSION
    ) {
        validate_table_schema(
            connection,
            path,
            chunk_claims::TABLE_NAME,
            chunk_claims::CREATE_TABLE,
        )?;
    }
    if matches!(
        schema_version,
        STAGING_CHECKPOINT_SCHEMA_VERSION
            | CHUNK_STAGING_SCHEMA_VERSION
            | RECORDING_CHUNKS_SCHEMA_VERSION
            | DATABASE_CLAIMS_SCHEMA_VERSION
            | DATABASE_ATTEMPTS_SCHEMA_VERSION
            | JOURNAL_SCHEMA_VERSION
    ) {
        validate_table_schema(
            connection,
            path,
            staging_checkpoints::TABLE_NAME,
            staging_checkpoints::CREATE_TABLE,
        )?;
    }
    if schema_version >= CHUNK_STAGING_SCHEMA_VERSION {
        validate_table_schema(
            connection,
            path,
            chunk_staging::TABLE_NAME,
            chunk_staging::CREATE_TABLE,
        )?;
    }
    if schema_version >= RECORDING_CHUNKS_SCHEMA_VERSION {
        validate_table_schema(
            connection,
            path,
            recording_chunks::TABLE_NAME,
            recording_chunks::CREATE_TABLE,
        )?;
    }
    if schema_version >= DATABASE_CLAIMS_SCHEMA_VERSION {
        validate_table_schema(
            connection,
            path,
            database_claims::BINDING_TABLE,
            database_claims::CREATE_BINDING,
        )?;
        validate_table_schema(
            connection,
            path,
            database_claims::SLICE_TABLE,
            database_claims::CREATE_SLICE,
        )?;
    }
    if schema_version >= DATABASE_ATTEMPTS_SCHEMA_VERSION {
        validate_table_schema(
            connection,
            path,
            database_attempts::START_TABLE,
            database_attempts::CREATE_START,
        )?;
        validate_table_schema(
            connection,
            path,
            database_attempts::OUTCOME_TABLE,
            database_attempts::CREATE_OUTCOME,
        )?;
    }
    if schema_version == JOURNAL_SCHEMA_VERSION {
        validate_table_schema(
            connection,
            path,
            publication_bindings::TABLE_NAME,
            publication_bindings::CREATE_TABLE,
        )?;
    }
    Ok(())
}

fn validate_table_schema(
    connection: &Connection,
    path: &Path,
    table_name: &str,
    expected_sql: &str,
) -> Result<(), OperationJournalStoreError> {
    let table_sql: String = connection.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![table_name],
        |row| row.get(0),
    )?;
    if normalize_sql(&table_sql) != normalize_sql(expected_sql) {
        return Err(audit_error(
            path,
            format!("existing {table_name} table schema differs"),
        ));
    }

    let columns: Vec<(String, String, i64, i64)> = connection
        .prepare(&format!("PRAGMA table_info({table_name})"))?
        .query_map([], |row| {
            Ok((row.get(1)?, row.get(2)?, row.get(3)?, row.get(5)?))
        })?
        .take(MAX_SCHEMA_COLUMNS)
        .collect::<Result<Vec<_>, _>>()?;
    let expected_columns = match table_name {
        database_attempts::START_TABLE => [
            ("attempt_id", "TEXT", 1, 1),
            ("operation_id", "TEXT", 1, 0),
            ("binding_id", "TEXT", 1, 0),
            ("start_nonce", "TEXT", 1, 0),
            ("database_slice", "TEXT", 1, 0),
            ("sidecar_slice", "TEXT", 1, 0),
            ("database_bytes", "INTEGER", 1, 0),
            ("wal_bytes", "INTEGER", 1, 0),
            ("shm_bytes", "INTEGER", 1, 0),
            ("image", "BLOB", 1, 0),
        ]
        .into_iter()
        .map(|(n, t, a, b)| (n.into(), t.into(), a, b))
        .collect(),
        database_attempts::OUTCOME_TABLE => [
            ("attempt_id", "TEXT", 1, 1),
            ("operation_id", "TEXT", 1, 0),
            ("observation_id", "TEXT", 1, 0),
            ("start_digest", "BLOB", 1, 0),
            ("image", "BLOB", 1, 0),
        ]
        .into_iter()
        .map(|(n, t, a, b)| (n.into(), t.into(), a, b))
        .collect(),
        database_claims::BINDING_TABLE => [
            ("binding_id", "TEXT", 1, 1),
            ("operation_id", "TEXT", 1, 0),
            ("ordinal", "INTEGER", 1, 0),
            ("role", "INTEGER", 1, 0),
            ("volume", "TEXT", 1, 0),
            ("main_file", "TEXT", 1, 0),
            ("decision_nonce", "TEXT", 1, 0),
            ("inventory_digest", "BLOB", 1, 0),
            ("inventory", "BLOB", 0, 0),
        ]
        .into_iter()
        .map(|(n, t, a, b)| (n.into(), t.into(), a, b))
        .collect(),
        database_claims::SLICE_TABLE => [
            ("slice_id", "TEXT", 1, 1),
            ("operation_id", "TEXT", 1, 0),
            ("binding_id", "TEXT", 1, 0),
            ("claim_id", "TEXT", 1, 0),
            ("participant_id", "TEXT", 1, 0),
            ("direction", "INTEGER", 1, 0),
            ("class", "INTEGER", 1, 0),
            ("ceiling", "INTEGER", 1, 0),
            ("wal_ceiling", "INTEGER", 1, 0),
            ("shm_ceiling", "INTEGER", 1, 0),
        ]
        .into_iter()
        .map(|(n, t, a, b)| (n.into(), t.into(), a, b))
        .collect(),
        recording_chunks::TABLE_NAME => vec![
            (String::from("chunk_id"), String::from("TEXT"), 1, 1),
            (String::from("operation_id"), String::from("TEXT"), 1, 0),
            (String::from("slot"), String::from("INTEGER"), 1, 0),
            (String::from("sequence"), String::from("INTEGER"), 1, 0),
            (String::from("record"), String::from("BLOB"), 1, 0),
        ],
        JOURNAL_TABLE_NAME | FULL_PROGRESS_TABLE_NAME => vec![
            (String::from("operation_id"), String::from("TEXT"), 1, 1),
            (String::from("sequence"), String::from("INTEGER"), 1, 0),
            (String::from("record"), String::from("BLOB"), 1, 0),
        ],
        recovery_reserve::CHARGE_TABLE_NAME => vec![
            (String::from("charge_id"), String::from("TEXT"), 1, 1),
            (String::from("volume_device"), String::from("TEXT"), 1, 0),
            (
                String::from("active_volume_device"),
                String::from("TEXT"),
                0,
                0,
            ),
            (String::from("operation_id"), String::from("TEXT"), 1, 0),
            (String::from("purpose"), String::from("TEXT"), 1, 0),
            (String::from("budget_bytes"), String::from("INTEGER"), 1, 0),
            (String::from("margin_bytes"), String::from("INTEGER"), 1, 0),
            (
                String::from("precharge_capacity_bytes"),
                String::from("INTEGER"),
                1,
                0,
            ),
            (
                String::from("remaining_bytes"),
                String::from("INTEGER"),
                1,
                0,
            ),
            (String::from("state"), String::from("TEXT"), 1, 0),
            (
                String::from("transition_sequence"),
                String::from("INTEGER"),
                1,
                0,
            ),
        ],
        recovery_reserve::TRANSITION_TABLE_NAME => vec![
            (String::from("charge_id"), String::from("TEXT"), 1, 1),
            (String::from("transition_id"), String::from("TEXT"), 1, 2),
            (
                String::from("transition_sequence"),
                String::from("INTEGER"),
                1,
                0,
            ),
            (String::from("kind"), String::from("TEXT"), 1, 0),
            (String::from("amount_bytes"), String::from("INTEGER"), 1, 0),
            (
                String::from("remaining_bytes"),
                String::from("INTEGER"),
                1,
                0,
            ),
            (String::from("state"), String::from("TEXT"), 1, 0),
        ],
        recovery_reserve_binding::TABLE_NAME => vec![
            (String::from("volume_device"), String::from("TEXT"), 1, 1),
            (String::from("profile_identity"), String::from("TEXT"), 1, 0),
            (String::from("root_identity"), String::from("TEXT"), 1, 0),
            (String::from("file_identity"), String::from("TEXT"), 1, 0),
            (
                String::from("allocation_unit"),
                String::from("INTEGER"),
                1,
                0,
            ),
            (
                String::from("protected_floor_bytes"),
                String::from("INTEGER"),
                1,
                0,
            ),
            (
                String::from("namespace_version"),
                String::from("INTEGER"),
                1,
                0,
            ),
            (String::from("namespace_name"), String::from("TEXT"), 1, 0),
            (
                String::from("owner_generation"),
                String::from("INTEGER"),
                1,
                0,
            ),
        ],
        ordinary_claims::TABLE_NAME => vec![
            (String::from("claim_id"), String::from("TEXT"), 1, 1),
            (String::from("operation_id"), String::from("TEXT"), 1, 0),
            (String::from("volume_device"), String::from("TEXT"), 1, 0),
            (String::from("allocation_class"), String::from("TEXT"), 1, 0),
            (
                String::from("allocated_bytes"),
                String::from("INTEGER"),
                1,
                0,
            ),
        ],
        ordinary_claims::RELEASE_TABLE_NAME => vec![
            (String::from("claim_id"), String::from("TEXT"), 1, 1),
            (String::from("operation_id"), String::from("TEXT"), 1, 0),
            (String::from("release_id"), String::from("TEXT"), 1, 0),
        ],
        chunk_claims::TABLE_NAME => vec![
            (String::from("chunk_id"), String::from("TEXT"), 1, 1),
            (String::from("operation_id"), String::from("TEXT"), 1, 0),
            (String::from("claim_id"), String::from("TEXT"), 1, 0),
            (String::from("participant_id"), String::from("TEXT"), 1, 0),
            (String::from("output_key"), String::from("TEXT"), 1, 0),
            (
                String::from("logical_offset"),
                String::from("INTEGER"),
                1,
                0,
            ),
            (
                String::from("logical_length"),
                String::from("INTEGER"),
                1,
                0,
            ),
            (String::from("volume_device"), String::from("TEXT"), 1, 0),
            (String::from("allocation_class"), String::from("TEXT"), 1, 0),
            (
                String::from("allocated_bytes"),
                String::from("INTEGER"),
                1,
                0,
            ),
            (
                String::from("allocation_unit"),
                String::from("INTEGER"),
                1,
                0,
            ),
            (
                String::from("protected_free_bytes"),
                String::from("INTEGER"),
                1,
                0,
            ),
            (String::from("state"), String::from("TEXT"), 1, 0),
            (String::from("issue_id"), String::from("TEXT"), 0, 0),
            (String::from("release_id"), String::from("TEXT"), 0, 0),
        ],
        publication_bindings::TABLE_NAME => vec![
            (String::from("output_id"), String::from("TEXT"), 1, 1),
            (
                String::from("publication_effect_id"),
                String::from("TEXT"),
                1,
                0,
            ),
            (String::from("record"), String::from("BLOB"), 1, 0),
        ],
        staging_checkpoints::TABLE_NAME => vec![
            (String::from("output_id"), String::from("TEXT"), 1, 1),
            (String::from("sequence"), String::from("INTEGER"), 1, 0),
            (String::from("record"), String::from("BLOB"), 1, 0),
            (
                String::from("grant_reserved"),
                String::from("INTEGER"),
                1,
                0,
            ),
        ],
        chunk_staging::TABLE_NAME => vec![
            (String::from("chunk_id"), String::from("TEXT"), 1, 1),
            (String::from("sequence"), String::from("INTEGER"), 1, 0),
            (String::from("record"), String::from("BLOB"), 1, 0),
        ],
        _ => {
            return Err(audit_error(path, format!("unsupported table {table_name}")));
        }
    };
    if columns != expected_columns {
        return Err(audit_error(
            path,
            format!("existing {table_name} columns differ"),
        ));
    }

    let indexes: Vec<(i64, String, i64, String, i64)> = connection
        .prepare(&format!("PRAGMA index_list({table_name})"))?
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?
        .take(MAX_SCHEMA_INDEXES)
        .collect::<Result<Vec<_>, _>>()?;
    let mut origins: Vec<String> = indexes
        .iter()
        .filter(|(_, _, unique, _, partial)| *unique == 1 && *partial == 0)
        .map(|(_, _, _, origin, _)| origin.clone())
        .collect();
    origins.sort_unstable();
    let expected_index_count = if table_name == database_claims::BINDING_TABLE {
        5
    } else if matches!(
        table_name,
        recording_chunks::TABLE_NAME | database_attempts::START_TABLE
    ) {
        3
    } else if matches!(
        table_name,
        recovery_reserve_binding::TABLE_NAME | ordinary_claims::TABLE_NAME
    ) {
        1
    } else if table_name == chunk_claims::TABLE_NAME {
        2
    } else {
        2
    };
    let expected_origins: Vec<String> = if table_name == database_claims::BINDING_TABLE {
        vec![
            String::from("pk"),
            String::from("u"),
            String::from("u"),
            String::from("u"),
            String::from("u"),
        ]
    } else if matches!(
        table_name,
        recording_chunks::TABLE_NAME | database_attempts::START_TABLE
    ) {
        vec![String::from("pk"), String::from("u"), String::from("u")]
    } else if matches!(
        table_name,
        recovery_reserve_binding::TABLE_NAME | ordinary_claims::TABLE_NAME
    ) {
        vec![String::from("pk")]
    } else if table_name == chunk_claims::TABLE_NAME {
        vec![String::from("pk"), String::from("u")]
    } else {
        vec![String::from("pk"), String::from("u")]
    };
    if indexes.len() != expected_index_count || origins != expected_origins {
        return Err(audit_error(
            path,
            format!("existing {table_name} indexes differ"),
        ));
    }
    if matches!(
        table_name,
        database_attempts::START_TABLE
            | database_attempts::OUTCOME_TABLE
            | publication_bindings::TABLE_NAME
    ) {
        let mut actual = Vec::new();
        for (_, name, _, _, _) in &indexes {
            let columns = connection
                .prepare("SELECT name FROM pragma_index_info(?1) ORDER BY seqno LIMIT ?2")?
                .query_map(params![name, MAX_SCHEMA_COLUMNS as i64], |r| {
                    r.get::<_, String>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?;
            actual.push(columns);
        }
        let wanted = if table_name == database_attempts::START_TABLE {
            vec![
                vec!["attempt_id"],
                vec!["start_nonce"],
                vec!["operation_id", "attempt_id"],
            ]
        } else if table_name == database_attempts::OUTCOME_TABLE {
            vec![vec!["attempt_id"], vec!["observation_id"]]
        } else {
            vec![vec!["output_id"], vec!["publication_effect_id"]]
        };
        let mut wanted: Vec<Vec<String>> = wanted
            .into_iter()
            .map(|v| v.into_iter().map(str::to_owned).collect())
            .collect();
        actual.sort();
        wanted.sort();
        if actual != wanted {
            return Err(audit_error(path, "attempt index columns differ"));
        }
    }
    Ok(())
}

fn migrate_legacy_schema(
    connection: &Connection,
    database_root: &BoundDatabaseRoot,
    database_path: &Path,
    database_identity: &str,
    profile_guard: &WritableProfileGuard,
    rollback_journal: Option<&PreexistingRollbackJournal>,
    migration_failure: Option<
        &dyn Fn(MigrationTestBoundary, &Connection) -> Option<OperationJournalStoreError>,
    >,
) -> Result<(), OperationJournalStoreError> {
    validate_migration_guards(
        connection,
        database_root,
        database_path,
        database_identity,
        profile_guard,
        rollback_journal,
        LEGACY_JOURNAL_SCHEMA_VERSION,
    )?;
    if let Some(error) =
        migration_failure.and_then(|hook| hook(MigrationTestBoundary::BeforeMigration, connection))
    {
        return Err(error);
    }
    connection.execute_batch("BEGIN EXCLUSIVE")?;
    let result = (|| {
        validate_migration_guards(
            connection,
            database_root,
            database_path,
            database_identity,
            profile_guard,
            rollback_journal,
            LEGACY_JOURNAL_SCHEMA_VERSION,
        )?;
        validate_existing_schema(connection, database_path, LEGACY_JOURNAL_SCHEMA_VERSION)?;
        connection.execute_batch(CREATE_FULL_PROGRESS_TABLE)?;
        if let Some(error) = migration_failure
            .and_then(|hook| hook(MigrationTestBoundary::AfterFullProgressTable, connection))
        {
            return Err(error);
        }
        connection.execute_batch(recovery_reserve::CREATE_CHARGE_TABLE)?;
        connection.execute_batch(recovery_reserve::CREATE_TRANSITION_TABLE)?;
        connection.execute_batch(recovery_reserve_binding::CREATE_TABLE)?;
        connection.execute_batch(ordinary_claims::CREATE_TABLE)?;
        connection.execute_batch(ordinary_claims::CREATE_RELEASE_TABLE)?;
        connection.execute_batch(chunk_claims::CREATE_TABLE)?;
        connection.execute_batch(staging_checkpoints::CREATE_TABLE)?;
        connection.execute_batch(chunk_staging::CREATE_TABLE)?;
        connection.execute_batch(recording_chunks::CREATE_TABLE)?;
        database_claims::create(connection)?;
        database_attempts::create(connection)?;
        publication_bindings::create(connection)?;
        connection.execute_batch(&format!("PRAGMA user_version = {JOURNAL_SCHEMA_VERSION};"))?;
        if let Some(error) =
            migration_failure.and_then(|hook| hook(MigrationTestBoundary::BeforeCommit, connection))
        {
            return Err(error);
        }
        validate_existing_schema(connection, database_path, JOURNAL_SCHEMA_VERSION)?;
        validate_migration_guards(
            connection,
            database_root,
            database_path,
            database_identity,
            profile_guard,
            rollback_journal,
            JOURNAL_SCHEMA_VERSION,
        )?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            if let Err(error) = connection.execute_batch("COMMIT") {
                let _ = connection.execute_batch("ROLLBACK");
                Err(error.into())
            } else {
                Ok(())
            }
        }
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

fn migrate_previous_schema(
    connection: &Connection,
    database_root: &BoundDatabaseRoot,
    database_path: &Path,
    database_identity: &str,
    profile_guard: &WritableProfileGuard,
    rollback_journal: Option<&PreexistingRollbackJournal>,
) -> Result<(), OperationJournalStoreError> {
    validate_migration_guards(
        connection,
        database_root,
        database_path,
        database_identity,
        profile_guard,
        rollback_journal,
        PREVIOUS_JOURNAL_SCHEMA_VERSION,
    )?;
    connection.execute_batch("BEGIN EXCLUSIVE")?;
    let result = (|| {
        validate_migration_guards(
            connection,
            database_root,
            database_path,
            database_identity,
            profile_guard,
            rollback_journal,
            PREVIOUS_JOURNAL_SCHEMA_VERSION,
        )?;
        validate_existing_schema(connection, database_path, PREVIOUS_JOURNAL_SCHEMA_VERSION)?;
        connection.execute_batch(recovery_reserve::CREATE_CHARGE_TABLE)?;
        connection.execute_batch(recovery_reserve::CREATE_TRANSITION_TABLE)?;
        connection.execute_batch(recovery_reserve_binding::CREATE_TABLE)?;
        connection.execute_batch(ordinary_claims::CREATE_TABLE)?;
        connection.execute_batch(ordinary_claims::CREATE_RELEASE_TABLE)?;
        connection.execute_batch(chunk_claims::CREATE_TABLE)?;
        connection.execute_batch(staging_checkpoints::CREATE_TABLE)?;
        connection.execute_batch(chunk_staging::CREATE_TABLE)?;
        connection.execute_batch(recording_chunks::CREATE_TABLE)?;
        database_claims::create(connection)?;
        database_attempts::create(connection)?;
        publication_bindings::create(connection)?;
        connection.execute_batch(&format!("PRAGMA user_version = {JOURNAL_SCHEMA_VERSION};"))?;
        validate_existing_schema(connection, database_path, JOURNAL_SCHEMA_VERSION)?;
        validate_migration_guards(
            connection,
            database_root,
            database_path,
            database_identity,
            profile_guard,
            rollback_journal,
            JOURNAL_SCHEMA_VERSION,
        )?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            if let Err(error) = connection.execute_batch("COMMIT") {
                let _ = connection.execute_batch("ROLLBACK");
                Err(error.into())
            } else {
                Ok(())
            }
        }
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

fn migrate_recovery_reserve_schema(
    connection: &Connection,
    database_root: &BoundDatabaseRoot,
    database_path: &Path,
    database_identity: &str,
    profile_guard: &WritableProfileGuard,
    rollback_journal: Option<&PreexistingRollbackJournal>,
) -> Result<(), OperationJournalStoreError> {
    validate_migration_guards(
        connection,
        database_root,
        database_path,
        database_identity,
        profile_guard,
        rollback_journal,
        RECOVERY_RESERVE_SCHEMA_VERSION,
    )?;
    connection.execute_batch("BEGIN EXCLUSIVE")?;
    let result = (|| {
        validate_migration_guards(
            connection,
            database_root,
            database_path,
            database_identity,
            profile_guard,
            rollback_journal,
            RECOVERY_RESERVE_SCHEMA_VERSION,
        )?;
        validate_existing_schema(connection, database_path, RECOVERY_RESERVE_SCHEMA_VERSION)?;
        connection.execute_batch(recovery_reserve_binding::CREATE_TABLE)?;
        connection.execute_batch(ordinary_claims::CREATE_TABLE)?;
        connection.execute_batch(ordinary_claims::CREATE_RELEASE_TABLE)?;
        connection.execute_batch(chunk_claims::CREATE_TABLE)?;
        connection.execute_batch(staging_checkpoints::CREATE_TABLE)?;
        connection.execute_batch(chunk_staging::CREATE_TABLE)?;
        connection.execute_batch(recording_chunks::CREATE_TABLE)?;
        database_claims::create(connection)?;
        database_attempts::create(connection)?;
        publication_bindings::create(connection)?;
        connection.execute_batch(&format!("PRAGMA user_version = {JOURNAL_SCHEMA_VERSION};"))?;
        validate_existing_schema(connection, database_path, JOURNAL_SCHEMA_VERSION)?;
        validate_migration_guards(
            connection,
            database_root,
            database_path,
            database_identity,
            profile_guard,
            rollback_journal,
            JOURNAL_SCHEMA_VERSION,
        )?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            if let Err(error) = connection.execute_batch("COMMIT") {
                let _ = connection.execute_batch("ROLLBACK");
                Err(error.into())
            } else {
                Ok(())
            }
        }
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

fn migrate_ordinary_claim_schema(
    connection: &Connection,
    database_root: &BoundDatabaseRoot,
    database_path: &Path,
    database_identity: &str,
    profile_guard: &WritableProfileGuard,
    rollback_journal: Option<&PreexistingRollbackJournal>,
) -> Result<(), OperationJournalStoreError> {
    validate_migration_guards(
        connection,
        database_root,
        database_path,
        database_identity,
        profile_guard,
        rollback_journal,
        RECOVERY_RESERVE_BINDING_SCHEMA_VERSION,
    )?;
    connection.execute_batch("BEGIN EXCLUSIVE")?;
    let result = (|| {
        validate_migration_guards(
            connection,
            database_root,
            database_path,
            database_identity,
            profile_guard,
            rollback_journal,
            RECOVERY_RESERVE_BINDING_SCHEMA_VERSION,
        )?;
        validate_existing_schema(
            connection,
            database_path,
            RECOVERY_RESERVE_BINDING_SCHEMA_VERSION,
        )?;
        connection.execute_batch(ordinary_claims::CREATE_TABLE)?;
        connection.execute_batch(ordinary_claims::CREATE_RELEASE_TABLE)?;
        connection.execute_batch(chunk_claims::CREATE_TABLE)?;
        connection.execute_batch(staging_checkpoints::CREATE_TABLE)?;
        connection.execute_batch(chunk_staging::CREATE_TABLE)?;
        connection.execute_batch(recording_chunks::CREATE_TABLE)?;
        database_claims::create(connection)?;
        database_attempts::create(connection)?;
        publication_bindings::create(connection)?;
        connection.execute_batch(&format!("PRAGMA user_version = {JOURNAL_SCHEMA_VERSION};"))?;
        validate_existing_schema(connection, database_path, JOURNAL_SCHEMA_VERSION)?;
        validate_migration_guards(
            connection,
            database_root,
            database_path,
            database_identity,
            profile_guard,
            rollback_journal,
            JOURNAL_SCHEMA_VERSION,
        )?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            if let Err(error) = connection.execute_batch("COMMIT") {
                let _ = connection.execute_batch("ROLLBACK");
                Err(error.into())
            } else {
                Ok(())
            }
        }
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

fn migrate_ordinary_claim_release_schema(
    connection: &Connection,
    database_root: &BoundDatabaseRoot,
    database_path: &Path,
    database_identity: &str,
    profile_guard: &WritableProfileGuard,
    rollback_journal: Option<&PreexistingRollbackJournal>,
) -> Result<(), OperationJournalStoreError> {
    validate_migration_guards(
        connection,
        database_root,
        database_path,
        database_identity,
        profile_guard,
        rollback_journal,
        ORDINARY_CLAIM_SCHEMA_VERSION,
    )?;
    connection.execute_batch("BEGIN EXCLUSIVE")?;
    let result = (|| {
        validate_migration_guards(
            connection,
            database_root,
            database_path,
            database_identity,
            profile_guard,
            rollback_journal,
            ORDINARY_CLAIM_SCHEMA_VERSION,
        )?;
        validate_existing_schema(connection, database_path, ORDINARY_CLAIM_SCHEMA_VERSION)?;
        connection.execute_batch(ordinary_claims::CREATE_RELEASE_TABLE)?;
        connection.execute_batch(chunk_claims::CREATE_TABLE)?;
        connection.execute_batch(staging_checkpoints::CREATE_TABLE)?;
        connection.execute_batch(chunk_staging::CREATE_TABLE)?;
        connection.execute_batch(recording_chunks::CREATE_TABLE)?;
        database_claims::create(connection)?;
        database_attempts::create(connection)?;
        publication_bindings::create(connection)?;
        connection.execute_batch(&format!("PRAGMA user_version = {JOURNAL_SCHEMA_VERSION};"))?;
        validate_existing_schema(connection, database_path, JOURNAL_SCHEMA_VERSION)?;
        validate_migration_guards(
            connection,
            database_root,
            database_path,
            database_identity,
            profile_guard,
            rollback_journal,
            JOURNAL_SCHEMA_VERSION,
        )?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            if let Err(error) = connection.execute_batch("COMMIT") {
                let _ = connection.execute_batch("ROLLBACK");
                Err(error.into())
            } else {
                Ok(())
            }
        }
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

fn migrate_chunk_claim_schema(
    connection: &Connection,
    database_root: &BoundDatabaseRoot,
    database_path: &Path,
    database_identity: &str,
    profile_guard: &WritableProfileGuard,
    rollback_journal: Option<&PreexistingRollbackJournal>,
) -> Result<(), OperationJournalStoreError> {
    validate_migration_guards(
        connection,
        database_root,
        database_path,
        database_identity,
        profile_guard,
        rollback_journal,
        ORDINARY_CLAIM_RELEASE_SCHEMA_VERSION,
    )?;
    connection.execute_batch("BEGIN EXCLUSIVE")?;
    let result = (|| {
        validate_migration_guards(
            connection,
            database_root,
            database_path,
            database_identity,
            profile_guard,
            rollback_journal,
            ORDINARY_CLAIM_RELEASE_SCHEMA_VERSION,
        )?;
        validate_existing_schema(
            connection,
            database_path,
            ORDINARY_CLAIM_RELEASE_SCHEMA_VERSION,
        )?;
        connection.execute_batch(chunk_claims::CREATE_TABLE)?;
        connection.execute_batch(staging_checkpoints::CREATE_TABLE)?;
        connection.execute_batch(chunk_staging::CREATE_TABLE)?;
        connection.execute_batch(recording_chunks::CREATE_TABLE)?;
        database_claims::create(connection)?;
        database_attempts::create(connection)?;
        publication_bindings::create(connection)?;
        connection.execute_batch(&format!("PRAGMA user_version = {JOURNAL_SCHEMA_VERSION};"))?;
        validate_existing_schema(connection, database_path, JOURNAL_SCHEMA_VERSION)?;
        validate_migration_guards(
            connection,
            database_root,
            database_path,
            database_identity,
            profile_guard,
            rollback_journal,
            JOURNAL_SCHEMA_VERSION,
        )?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            if let Err(error) = connection.execute_batch("COMMIT") {
                let _ = connection.execute_batch("ROLLBACK");
                Err(error.into())
            } else {
                Ok(())
            }
        }
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

fn migrate_staging_checkpoint_schema(
    connection: &Connection,
    database_root: &BoundDatabaseRoot,
    database_path: &Path,
    database_identity: &str,
    profile_guard: &WritableProfileGuard,
    rollback_journal: Option<&PreexistingRollbackJournal>,
) -> Result<(), OperationJournalStoreError> {
    validate_migration_guards(
        connection,
        database_root,
        database_path,
        database_identity,
        profile_guard,
        rollback_journal,
        CHUNK_CLAIM_SCHEMA_VERSION,
    )?;
    connection.execute_batch("BEGIN EXCLUSIVE")?;
    let result = (|| {
        validate_migration_guards(
            connection,
            database_root,
            database_path,
            database_identity,
            profile_guard,
            rollback_journal,
            CHUNK_CLAIM_SCHEMA_VERSION,
        )?;
        validate_existing_schema(connection, database_path, CHUNK_CLAIM_SCHEMA_VERSION)?;
        connection.execute_batch(staging_checkpoints::CREATE_TABLE)?;
        connection.execute_batch(chunk_staging::CREATE_TABLE)?;
        connection.execute_batch(recording_chunks::CREATE_TABLE)?;
        database_claims::create(connection)?;
        database_attempts::create(connection)?;
        publication_bindings::create(connection)?;
        connection.execute_batch(&format!("PRAGMA user_version = {JOURNAL_SCHEMA_VERSION};"))?;
        validate_existing_schema(connection, database_path, JOURNAL_SCHEMA_VERSION)?;
        validate_migration_guards(
            connection,
            database_root,
            database_path,
            database_identity,
            profile_guard,
            rollback_journal,
            JOURNAL_SCHEMA_VERSION,
        )?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            if let Err(error) = connection.execute_batch("COMMIT") {
                let _ = connection.execute_batch("ROLLBACK");
                Err(error.into())
            } else {
                Ok(())
            }
        }
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

fn migrate_chunk_staging_schema(
    connection: &Connection,
    database_root: &BoundDatabaseRoot,
    database_path: &Path,
    database_identity: &str,
    profile_guard: &WritableProfileGuard,
    rollback_journal: Option<&PreexistingRollbackJournal>,
) -> Result<(), OperationJournalStoreError> {
    validate_migration_guards(
        connection,
        database_root,
        database_path,
        database_identity,
        profile_guard,
        rollback_journal,
        STAGING_CHECKPOINT_SCHEMA_VERSION,
    )?;
    connection.execute_batch("BEGIN EXCLUSIVE")?;
    let result = (|| {
        validate_migration_guards(
            connection,
            database_root,
            database_path,
            database_identity,
            profile_guard,
            rollback_journal,
            STAGING_CHECKPOINT_SCHEMA_VERSION,
        )?;
        validate_existing_schema(connection, database_path, STAGING_CHECKPOINT_SCHEMA_VERSION)?;
        connection.execute_batch(chunk_staging::CREATE_TABLE)?;
        connection.execute_batch(recording_chunks::CREATE_TABLE)?;
        database_claims::create(connection)?;
        database_attempts::create(connection)?;
        publication_bindings::create(connection)?;
        connection.execute_batch(&format!("PRAGMA user_version = {JOURNAL_SCHEMA_VERSION};"))?;
        validate_existing_schema(connection, database_path, JOURNAL_SCHEMA_VERSION)?;
        validate_migration_guards(
            connection,
            database_root,
            database_path,
            database_identity,
            profile_guard,
            rollback_journal,
            JOURNAL_SCHEMA_VERSION,
        )?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            if let Err(error) = connection.execute_batch("COMMIT") {
                let _ = connection.execute_batch("ROLLBACK");
                Err(error.into())
            } else {
                Ok(())
            }
        }
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

fn migrate_recording_chunks_schema(
    connection: &Connection,
    database_root: &BoundDatabaseRoot,
    database_path: &Path,
    database_identity: &str,
    profile_guard: &WritableProfileGuard,
    rollback_journal: Option<&PreexistingRollbackJournal>,
) -> Result<(), OperationJournalStoreError> {
    validate_migration_guards(
        connection,
        database_root,
        database_path,
        database_identity,
        profile_guard,
        rollback_journal,
        CHUNK_STAGING_SCHEMA_VERSION,
    )?;
    connection.execute_batch("BEGIN EXCLUSIVE")?;
    let result = (|| {
        validate_migration_guards(
            connection,
            database_root,
            database_path,
            database_identity,
            profile_guard,
            rollback_journal,
            CHUNK_STAGING_SCHEMA_VERSION,
        )?;
        validate_existing_schema(connection, database_path, CHUNK_STAGING_SCHEMA_VERSION)?;
        connection.execute_batch(recording_chunks::CREATE_TABLE)?;
        database_claims::create(connection)?;
        database_attempts::create(connection)?;
        publication_bindings::create(connection)?;
        connection.execute_batch(&format!("PRAGMA user_version = {JOURNAL_SCHEMA_VERSION};"))?;
        validate_existing_schema(connection, database_path, JOURNAL_SCHEMA_VERSION)?;
        validate_migration_guards(
            connection,
            database_root,
            database_path,
            database_identity,
            profile_guard,
            rollback_journal,
            JOURNAL_SCHEMA_VERSION,
        )?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            if let Err(error) = connection.execute_batch("COMMIT") {
                let _ = connection.execute_batch("ROLLBACK");
                Err(error.into())
            } else {
                Ok(())
            }
        }
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

fn migrate_database_claim_schema(
    connection: &Connection,
    database_root: &BoundDatabaseRoot,
    database_path: &Path,
    database_identity: &str,
    profile_guard: &WritableProfileGuard,
    rollback_journal: Option<&PreexistingRollbackJournal>,
) -> Result<(), OperationJournalStoreError> {
    validate_migration_guards(
        connection,
        database_root,
        database_path,
        database_identity,
        profile_guard,
        rollback_journal,
        RECORDING_CHUNKS_SCHEMA_VERSION,
    )?;
    connection.execute_batch("BEGIN EXCLUSIVE")?;
    let result = (|| {
        validate_migration_guards(
            connection,
            database_root,
            database_path,
            database_identity,
            profile_guard,
            rollback_journal,
            RECORDING_CHUNKS_SCHEMA_VERSION,
        )?;
        validate_existing_schema(connection, database_path, RECORDING_CHUNKS_SCHEMA_VERSION)?;
        database_claims::create(connection)?;
        database_attempts::create(connection)?;
        publication_bindings::create(connection)?;
        connection.execute_batch(&format!("PRAGMA user_version = {JOURNAL_SCHEMA_VERSION};"))?;
        validate_existing_schema(connection, database_path, JOURNAL_SCHEMA_VERSION)?;
        validate_migration_guards(
            connection,
            database_root,
            database_path,
            database_identity,
            profile_guard,
            rollback_journal,
            JOURNAL_SCHEMA_VERSION,
        )?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            if let Err(error) = connection.execute_batch("COMMIT") {
                let _ = connection.execute_batch("ROLLBACK");
                Err(error.into())
            } else {
                Ok(())
            }
        }
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

fn migrate_database_attempt_schema(
    connection: &Connection,
    database_root: &BoundDatabaseRoot,
    database_path: &Path,
    database_identity: &str,
    profile_guard: &WritableProfileGuard,
    rollback_journal: Option<&PreexistingRollbackJournal>,
) -> Result<(), OperationJournalStoreError> {
    validate_migration_guards(
        connection,
        database_root,
        database_path,
        database_identity,
        profile_guard,
        rollback_journal,
        DATABASE_CLAIMS_SCHEMA_VERSION,
    )?;
    connection.execute_batch("BEGIN EXCLUSIVE")?;
    let result = (|| {
        validate_migration_guards(
            connection,
            database_root,
            database_path,
            database_identity,
            profile_guard,
            rollback_journal,
            DATABASE_CLAIMS_SCHEMA_VERSION,
        )?;
        validate_existing_schema(connection, database_path, DATABASE_CLAIMS_SCHEMA_VERSION)?;
        database_attempts::create(connection)?;
        publication_bindings::create(connection)?;
        connection.execute_batch(&format!("PRAGMA user_version = {JOURNAL_SCHEMA_VERSION};"))?;
        validate_existing_schema(connection, database_path, JOURNAL_SCHEMA_VERSION)?;
        validate_migration_guards(
            connection,
            database_root,
            database_path,
            database_identity,
            profile_guard,
            rollback_journal,
            JOURNAL_SCHEMA_VERSION,
        )?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            if let Err(error) = connection.execute_batch("COMMIT") {
                let _ = connection.execute_batch("ROLLBACK");
                Err(error.into())
            } else {
                Ok(())
            }
        }
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

fn migrate_publication_binding_schema(
    connection: &Connection,
    database_root: &BoundDatabaseRoot,
    database_path: &Path,
    database_identity: &str,
    profile_guard: &WritableProfileGuard,
    rollback_journal: Option<&PreexistingRollbackJournal>,
) -> Result<(), OperationJournalStoreError> {
    validate_migration_guards(
        connection,
        database_root,
        database_path,
        database_identity,
        profile_guard,
        rollback_journal,
        DATABASE_ATTEMPTS_SCHEMA_VERSION,
    )?;
    connection.execute_batch("BEGIN EXCLUSIVE")?;
    let result = (|| {
        validate_migration_guards(
            connection,
            database_root,
            database_path,
            database_identity,
            profile_guard,
            rollback_journal,
            DATABASE_ATTEMPTS_SCHEMA_VERSION,
        )?;
        validate_existing_schema(connection, database_path, DATABASE_ATTEMPTS_SCHEMA_VERSION)?;
        publication_bindings::create(connection)?;
        connection.execute_batch(&format!("PRAGMA user_version = {JOURNAL_SCHEMA_VERSION};"))?;
        validate_existing_schema(connection, database_path, JOURNAL_SCHEMA_VERSION)?;
        validate_migration_guards(
            connection,
            database_root,
            database_path,
            database_identity,
            profile_guard,
            rollback_journal,
            JOURNAL_SCHEMA_VERSION,
        )?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            if let Err(error) = connection.execute_batch("COMMIT") {
                let _ = connection.execute_batch("ROLLBACK");
                Err(error.into())
            } else {
                Ok(())
            }
        }
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

fn validate_migration_guards(
    connection: &Connection,
    database_root: &BoundDatabaseRoot,
    database_path: &Path,
    database_identity: &str,
    profile_guard: &WritableProfileGuard,
    rollback_journal: Option<&PreexistingRollbackJournal>,
    expected_schema_version: u32,
) -> Result<(), OperationJournalStoreError> {
    database_root.validate_profile_guard(profile_guard)?;
    validate_database_file_identity(database_root, database_path, database_identity)?;
    validate_connection_identity(connection, database_root, database_path, database_identity)?;
    verify_connection_configuration(connection, database_path, expected_schema_version)?;
    reject_unexpected_sidecars(database_root, database_path, true)?;
    if let Some(rollback_journal) = rollback_journal {
        validate_rollback_journal_identity(
            database_root,
            database_path,
            &rollback_journal.identity,
        )?;
    }
    Ok(())
}

fn normalize_sql(sql: &str) -> String {
    sql.split_whitespace()
        .collect::<String>()
        .to_ascii_lowercase()
}

fn configure_connection(connection: &Connection) -> Result<(), OperationJournalStoreError> {
    connection.execute_batch(
        "PRAGMA journal_mode = DELETE;
         PRAGMA synchronous = EXTRA;
         PRAGMA locking_mode = EXCLUSIVE;
         PRAGMA foreign_keys = ON;",
    )?;
    Ok(())
}

fn bootstrap_schema(connection: &Connection) -> Result<(), OperationJournalStoreError> {
    connection.execute_batch("BEGIN EXCLUSIVE")?;
    let result = connection
        .execute_batch(&format!(
            "PRAGMA application_id = {JOURNAL_APPLICATION_ID};
             PRAGMA user_version = {JOURNAL_SCHEMA_VERSION};
             {CREATE_JOURNAL_TABLE};
             {CREATE_FULL_PROGRESS_TABLE};
             {charge_table};
             {transition_table};
             {binding_table};
             {ordinary_claim_table};
             {ordinary_claim_release_table};
             {chunk_claim_table};
             {staging_checkpoint_table};
             {chunk_staging_table};
             {recording_chunks_table};
             {database_binding_table};
             {database_slice_table};
             {database_attempt_table};
             {database_attempt_outcome_table};
             {publication_issuance_table};",
            charge_table = recovery_reserve::CREATE_CHARGE_TABLE,
            transition_table = recovery_reserve::CREATE_TRANSITION_TABLE,
            binding_table = recovery_reserve_binding::CREATE_TABLE,
            ordinary_claim_table = ordinary_claims::CREATE_TABLE,
            ordinary_claim_release_table = ordinary_claims::CREATE_RELEASE_TABLE,
            chunk_claim_table = chunk_claims::CREATE_TABLE,
            staging_checkpoint_table = staging_checkpoints::CREATE_TABLE,
            chunk_staging_table = chunk_staging::CREATE_TABLE,
            recording_chunks_table = recording_chunks::CREATE_TABLE,
            database_binding_table = database_claims::CREATE_BINDING,
            database_slice_table = database_claims::CREATE_SLICE,
            database_attempt_table = database_attempts::CREATE_START,
            database_attempt_outcome_table = database_attempts::CREATE_OUTCOME,
            publication_issuance_table = publication_bindings::CREATE_TABLE,
        ))
        .map(|_| ());
    match result {
        Ok(()) => {
            if let Err(error) = connection.execute_batch("COMMIT") {
                let _ = connection.execute_batch("ROLLBACK");
                Err(error.into())
            } else {
                Ok(())
            }
        }
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK");
            Err(error.into())
        }
    }
}

fn verify_connection_configuration(
    connection: &Connection,
    path: &Path,
    expected_schema_version: u32,
) -> Result<(), OperationJournalStoreError> {
    let journal_mode: String =
        connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    let locking_mode: String =
        connection.pragma_query_value(None, "locking_mode", |row| row.get(0))?;
    let synchronous: i64 = connection.pragma_query_value(None, "synchronous", |row| row.get(0))?;
    let application_id: i64 =
        connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
    let user_version: i64 =
        connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("delete")
        || !locking_mode.eq_ignore_ascii_case("exclusive")
        || synchronous != 3
        || application_id != i64::from(JOURNAL_APPLICATION_ID)
        || user_version != i64::from(expected_schema_version)
    {
        return Err(audit_error(
            path,
            format!(
                "SQLite configuration is journal_mode={journal_mode}, locking_mode={locking_mode}, synchronous={synchronous}, application_id={application_id:#x}, user_version={user_version}"
            ),
        ));
    }
    Ok(())
}

fn validate_database_file_identity(
    root: &BoundDatabaseRoot,
    database_path: &Path,
    expected_identity: &str,
) -> Result<(), OperationJournalStoreError> {
    sqlite_identity::validate_database_file_identity_with_unavailable_reason(
        root,
        Path::new(JOURNAL_DATABASE_FILE_NAME),
        database_path,
        expected_identity,
        "stable identity is unavailable for the open journal file",
    )
    .map_err(map_identity_error)
}

fn validate_connection_identity(
    connection: &Connection,
    _root: &BoundDatabaseRoot,
    path: &Path,
    expected_identity: &str,
) -> Result<(), OperationJournalStoreError> {
    sqlite_identity::validate_connection_identity(connection, path, expected_identity)
        .map_err(map_identity_error)
}

fn map_identity_error(error: DatabaseIdentityError) -> OperationJournalStoreError {
    match error {
        DatabaseIdentityError::Io { path, source } => io_error(&path, source),
        DatabaseIdentityError::NotRegular { path } => {
            audit_error(&path, "journal database is not a regular file")
        }
        DatabaseIdentityError::Replaced { path } => {
            OperationJournalStoreError::DatabaseReplaced { path }
        }
        DatabaseIdentityError::Unavailable { reason, .. } => {
            OperationJournalStoreError::IdentityCheckUnavailable { reason }
        }
        DatabaseIdentityError::ConnectionMismatch { path, reason } => {
            OperationJournalStoreError::ConnectionIdentityMismatch { path, reason }
        }
        DatabaseIdentityError::UnsafeSidecar { path, reason } => audit_error(&path, reason),
    }
}

fn io_error(path: &Path, source: io::Error) -> OperationJournalStoreError {
    OperationJournalStoreError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn audit_error(path: &Path, reason: impl Into<String>) -> OperationJournalStoreError {
    OperationJournalStoreError::RequiresAudit {
        path: path.to_path_buf(),
        reason: reason.into(),
    }
}

fn retained_error(
    operation_id: Option<OperationId>,
    reason: impl Into<String>,
) -> OperationJournalStoreError {
    OperationJournalStoreError::RetainedRecord {
        operation_id,
        reason: reason.into(),
    }
}

fn retained_decode_error(
    operation_id: OperationId,
    error: DecodeError,
) -> OperationJournalStoreError {
    retained_error(Some(operation_id), error.to_string())
}

impl From<EncodeError> for OperationJournalStoreError {
    fn from(error: EncodeError) -> Self {
        Self::InconsistentRecord {
            reason: error.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    include!("store/retry_telemetry_integration_tests.rs");
    include!("store/historical_claim_observation_tests.rs");
    include!("store/current_reserve_binding_bounds_tests.rs");
    include!("store/ordinary_rebuild_backing_tests.rs");
    include!("store/publication_binding_tests.rs");
    include!("store/database_claims_tests.rs");
    include!("store/recording_retention_tests.rs");
    include!("store/record_materialization_tests.rs");
    include!("store/recording_prestart_budget_tests.rs");
    include!("store/recording_control_tests.rs");
    include!("store/json_materialization_tests.rs");
    use std::collections::BTreeMap;
    use std::fs;
    #[cfg(unix)]
    use std::io::Write;
    #[cfg(target_os = "macos")]
    use std::os::unix::fs::MetadataExt;
    #[cfg(target_os = "macos")]
    use std::path::Path;
    use std::sync::mpsc;
    use std::thread;

    use tempfile::TempDir;
    use uuid::Uuid;

    use super::full_progress::FullProgressWriteOutcome;
    use super::*;
    use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard};
    use crate::durable_capacity::{
        CapacityAllocationClass, DurableCapacityPlan, DurableVolumeCapacity,
        PROTECTED_FREE_SPACE_FLOOR, VolumeFacts, VolumeIdentity,
    };
    use crate::operation_journal::capacity_provider::{
        CapacityProvider, CapacityProviderError, CapacityRebuildReadiness,
        ProvisionalLeaseDisposition,
    };
    use crate::operation_journal::{AdmissionClaim, AdmissionClaimId, AdmissionClaimInventory};
    use crate::operation_journal::{
        CopyCollisionPolicy, CopyEndpoint, CopyIntentArgs, CopySourceExpectation,
        CreateFolderIntentArgs, EvidenceBatchId, ExternalCaptureBoundary, ExternalNativeString,
        ExternalRawEventKind, ExternalRawNativeAccounting, ExternalRawObservation,
        ExternalRawObservationEnvelope, ExternalRawObservationLimits,
        ExternalRawObservationMetadata, ExternalRawObservationProvenance,
        ExternalReconciliationIntentArgs, ExternalRootBindingId, ExtractAndTrimIntentArgs,
        FiniteF32Bits, HistoryAction, HistoryDirection, HistoryRecoveryBackupObservation,
        HistoryRecoveryLeafObservation, HistoryRestoreTargetExpectation, HistoryScopeId,
        HistoryUndoRedoIntentArgs, HistoryWaveformRestore, ImportFileExpectation, ImportIntentArgs,
        NamespaceCommandArgs, NamespaceDirectoryEndpoint, NamespaceDirectoryExpectation,
        NamespaceDirectoryLocator, NamespaceEntryEndpoint, NamespaceEntryExpectation,
        NamespaceFileExpectation, NamespaceLeafName, NamespaceMoveIntentArgs,
        NamespaceRenameIntentArgs, NamespaceTarget, ParticipantCheckpoint, ParticipantDescriptor,
        ParticipantKind, ParticipantRequirement, ParticipantSet, RawWavExtractionRecipe,
        RecoveryCursor, RecoveryRowOutcome, RecoveryScanError, RemovalCommandArgs,
        RenderCommandArgs, RetainedDeleteId, RetainedFolderObservation, RetainedRecoveryRootKind,
        RetainedResolutionCommandArgs, RetainedResolutionStage, RetainedRestoreIntentArgs,
        RetryErrorClass, RetryLeaseId, RetryObservation, SCHEMA_V1, SCHEMA_V6, SCHEMA_V7,
        SCHEMA_V8, SCHEMA_V9, SCHEMA_V10, SCHEMA_V11, SCHEMA_V12, SCHEMA_V13, SCHEMA_V14,
        SCHEMA_V15, SCHEMA_V16, SelectionExtractIntentArgs, SelectionNormalizeIntentArgs,
        SelectionOutputCommandArgs, SelectionOutputRecipe, StoredRelativePath, StoredSourceFence,
        TrashIntentArgs, UniversalCommandArgs, UniversalCommandKind, UniversalIntent,
        WaveformDuplicateDoubleIntentArgs, WaveformEditFrameRange, WaveformEditIntentArgs,
        WaveformEditOperationArgs, WaveformEditSettingsArgs, WaveformFileRevisionObservation,
        WaveformNormalizeIntentArgs, WaveformOutputSpec, WaveformRenderOutputSpec,
        WaveformSampleFormat, WaveformSourceSpec, fingerprint_selected_entry_json,
    };
    use crate::sample_sources::{SourceId, SourcePlaybackType};
    use crate::test_runtime::TestRuntimeGuard;

    fn full_progress_store_records() -> (
        crate::operation_journal::full_progress_codec::StoredFullProgressRecord,
        crate::operation_journal::full_progress_codec::StoredFullProgressRecord,
    ) {
        let (initial, progressed) =
            crate::operation_journal::work_progress::test_full_progress_store_snapshots();
        (
            crate::operation_journal::full_progress_codec::StoredFullProgressRecord::new(initial)
                .expect("valid initial full-progress fixture"),
            crate::operation_journal::full_progress_codec::StoredFullProgressRecord::new(
                progressed,
            )
            .expect("valid progressed full-progress fixture"),
        )
    }

    fn full_progress_raw_row(
        store: &OperationJournalStore,
        operation_id: OperationId,
    ) -> (String, i64, Vec<u8>) {
        store
            .connection
            .query_row(
                "SELECT operation_id, sequence, record
                 FROM operation_journal_full_progress WHERE operation_id = ?1",
                params![operation_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read full-progress raw row")
    }

    include!("store/action_status_tests.rs");

    struct Fixture {
        owner: WritableProfileGuard,
        _profile: PersistenceProfileGuard,
        _config: ConfigBaseGuard,
        _base: TempDir,
        // Release test serialization only after handles, overrides, and files are gone.
        _runtime: TestRuntimeGuard,
    }

    fn fixture() -> Fixture {
        let runtime = TestRuntimeGuard::acquire();
        let base = tempfile::tempdir().expect("temporary config base");
        let base_path = fs::canonicalize(base.path()).expect("canonical temporary config base");
        let config = ConfigBaseGuard::set(base_path);
        let profile =
            PersistenceProfileGuard::named(format!("journal-store-{}", Uuid::new_v4().simple()));
        let owner = WritableProfileGuard::acquire_current().expect("profile owner");
        Fixture {
            _runtime: runtime,
            _base: base,
            _config: config,
            _profile: profile,
            owner,
        }
    }

    fn intent_record(operation_id: OperationId) -> StoredOperationRecord {
        let value = serde_json::json!({
            "schema_version": 1,
            "operation_id": operation_id.as_uuid(),
            "intent": {
                "actor": "User",
                "kind": "FileMutation",
                "label": "store-test"
            },
            "phase": "IntentDurable",
            "disposition": "None",
            "payload": {},
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
            "prepared": null,
            "staged": null,
            "published": null,
            "replacement_qualification": null,
            "created_unix_ms": 1,
            "updated_unix_ms": 1
        });
        decode(&serde_json::to_vec(&value).expect("serialize intent fixture"))
            .expect("decode intent fixture")
    }

    fn claim_for_intent(
        record: &StoredOperationRecord,
        claim_id: AdmissionClaimId,
        allocated_bytes: Option<u64>,
    ) -> AdmissionClaim {
        let row = &record
            .capacity_plan
            .as_ref()
            .expect("intent capacity plan")
            .volumes[0];
        AdmissionClaim::new(
            claim_id,
            row.identity.clone(),
            row.allocation_class.into(),
            allocated_bytes.unwrap_or(row.allocated_bytes),
        )
        .expect("intent claim")
    }

    fn claims_for_intent(record: &StoredOperationRecord) -> AdmissionClaimInventory {
        let rows = &record
            .capacity_plan
            .as_ref()
            .expect("intent capacity plan")
            .volumes;
        AdmissionClaimInventory::new(
            rows.iter()
                .map(|row| {
                    AdmissionClaim::new(
                        AdmissionClaimId::new(),
                        row.identity.clone(),
                        row.allocation_class.into(),
                        row.allocated_bytes,
                    )
                    .expect("intent claim")
                })
                .collect(),
        )
        .expect("intent claim inventory")
    }

    fn provider_for_intent(record: &StoredOperationRecord) -> CapacityProvider {
        let mut facts = BTreeMap::new();
        for row in &record
            .capacity_plan
            .as_ref()
            .expect("intent capacity plan")
            .volumes
        {
            facts.insert(
                row.identity.clone(),
                VolumeFacts {
                    identity: row.identity.clone(),
                    free_bytes: PROTECTED_FREE_SPACE_FLOOR
                        .checked_add(
                            row.allocated_bytes
                                .checked_mul(8)
                                .expect("fixture free-space range"),
                        )
                        .expect("fixture free-space range"),
                    allocation_unit: row.allocation_unit,
                },
            );
        }
        CapacityProvider::qualified_fixture(facts)
    }

    fn release_request(
        operation_id: OperationId,
        claim_id: AdmissionClaimId,
        release_id: Uuid,
    ) -> OrdinaryClaimReleaseRequest {
        OrdinaryClaimReleaseRequest::new(operation_id, claim_id, release_id)
            .expect("valid ordinary claim release request")
    }

    fn chunk_request_for_record(
        record: &StoredOperationRecord,
        chunk_id: ChunkId,
        claim_id: AdmissionClaimId,
        output_key: &str,
        logical_offset: u64,
    ) -> ChunkClaimRequest {
        let row = &record
            .capacity_plan
            .as_ref()
            .expect("chunk capacity plan")
            .volumes[0];
        let participant_id = record
            .universal_intent
            .as_ref()
            .expect("chunk universal intent")
            .participants()
            .descriptors()[0]
            .id();
        ChunkClaimRequest::new(
            chunk_id,
            record.operation_id,
            AdmissionClaim::new(
                claim_id,
                row.identity.clone(),
                row.allocation_class.into(),
                row.allocated_bytes,
            )
            .expect("chunk claim"),
            participant_id,
            output_key.to_owned(),
            logical_offset,
            row.logical_bytes,
            row.allocation_unit,
            row.protected_free_bytes,
        )
        .expect("chunk request")
    }

    fn ordinary_claim_count(store: &OperationJournalStore) -> usize {
        store
            .read_ordinary_claim_page(None, MAX_PAGE_ROWS)
            .expect("read ordinary claim page")
            .records()
            .len()
    }

    fn universal_intent_record(operation_id: OperationId) -> StoredOperationRecord {
        let value = serde_json::json!({
            "schema_version": 3,
            "operation_id": operation_id.as_uuid(),
            "intent": {
                "actor": "User",
                "kind": "FileMutation",
                "label": "store-v3-test"
            },
            "universal_intent": {
                "parent_operation_id": null,
                "command_kind": "CreateFile",
                "user_status_key": "operation.status.store-v3",
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
            "payload": {"store": "v3"},
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
            "created_unix_ms": 41,
            "updated_unix_ms": 42
        });
        decode(&serde_json::to_vec(&value).expect("serialize universal intent fixture"))
            .expect("decode universal intent fixture")
    }

    fn v5_copy_inputs() -> (
        OperationId,
        OperationIntent,
        UniversalIntent,
        CopyIntentArgs,
        DurableCapacityPlan,
    ) {
        let operation_id = OperationId::new();
        let source_id = SourceId::from_string("source-a");
        let source_participant_id = crate::operation_journal::ParticipantId::new();
        let filesystem_participant_id = crate::operation_journal::ParticipantId::new();
        let source_fence =
            StoredSourceFence::new(source_id.clone(), "root-a", "database-a", 1, 7, 1)
                .expect("source fence");
        let participants = ParticipantSet::new(
            vec![
                ParticipantDescriptor::new(
                    source_participant_id,
                    ParticipantKind::SourceDatabase,
                    ParticipantRequirement::Required,
                    Some(source_fence),
                    None,
                )
                .expect("source participant"),
                ParticipantDescriptor::new(
                    filesystem_participant_id,
                    ParticipantKind::Filesystem,
                    ParticipantRequirement::Required,
                    None,
                    None,
                )
                .expect("filesystem participant"),
            ],
            vec![
                ParticipantCheckpoint::new(
                    source_participant_id,
                    crate::operation_journal::ParticipantCheckpointState::Pending,
                    None,
                    None,
                )
                .expect("source checkpoint"),
                ParticipantCheckpoint::new(
                    filesystem_participant_id,
                    crate::operation_journal::ParticipantCheckpointState::Pending,
                    None,
                    None,
                )
                .expect("filesystem checkpoint"),
            ],
        )
        .expect("participants");
        let universal_intent = UniversalIntent::new(
            None,
            UniversalCommandKind::Copy,
            "operation.status.copy",
            participants,
        )
        .expect("copy universal intent");
        let source = CopyEndpoint::new(
            source_participant_id,
            source_id.clone(),
            StoredRelativePath::new("in.wav").expect("source path"),
        )
        .expect("source endpoint");
        let destination = CopyEndpoint::new(
            source_participant_id,
            source_id,
            StoredRelativePath::new("out.wav").expect("destination path"),
        )
        .expect("destination endpoint");
        let expected_source =
            CopySourceExpectation::new(7, "file-identity", 10, 11).expect("source expectation");
        let command_args = CopyIntentArgs::new(
            source,
            destination,
            expected_source,
            CopyCollisionPolicy::NoReplace,
            Some(String::from("metadata/entry")),
        )
        .expect("copy arguments");
        let capacity_plan = DurableCapacityPlan {
            volumes: vec![DurableVolumeCapacity {
                identity: VolumeIdentity { device: 1 },
                allocation_unit: 1,
                allocation_class: CapacityAllocationClass::DestinationStaging,
                logical_bytes: 1,
                allocated_bytes: 1,
                protected_free_bytes: PROTECTED_FREE_SPACE_FLOOR,
            }],
        };
        let intent = OperationIntent {
            actor: OperationActor::User,
            kind: OperationKind::FileMutation,
            label: String::from("copy store test"),
        };
        (
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
        )
    }

    fn v6_command_inputs(
        variant: UniversalCommandKind,
    ) -> (
        OperationId,
        OperationIntent,
        UniversalIntent,
        UniversalCommandArgs,
        DurableCapacityPlan,
    ) {
        let (operation_id, intent, source_universal, copy_args, capacity_plan) = v5_copy_inputs();
        let participants = source_universal.participants().clone();
        let source_participant_id = copy_args.source().participant_id();
        let filesystem_participant_id = participants
            .descriptors()
            .iter()
            .find(|descriptor| descriptor.kind() == ParticipantKind::Filesystem)
            .expect("filesystem descriptor")
            .id();
        let command_args = match variant {
            UniversalCommandKind::Copy => UniversalCommandArgs::Copy(copy_args),
            UniversalCommandKind::Duplicate => UniversalCommandArgs::DuplicateSame(copy_args),
            UniversalCommandKind::Import => UniversalCommandArgs::Import(
                ImportIntentArgs::new(
                    ExternalRootBindingId::new(),
                    filesystem_participant_id,
                    "external-root-identity",
                    StoredRelativePath::new("sample.wav").expect("external path"),
                    ImportFileExpectation::new("external-file-identity", 12, 34)
                        .expect("external file"),
                    CopyEndpoint::new(
                        source_participant_id,
                        SourceId::from_string("source-a"),
                        StoredRelativePath::new("out.wav").expect("destination path"),
                    )
                    .expect("destination endpoint"),
                    CopyCollisionPolicy::NoReplace,
                    Some(String::from("metadata/entry")),
                )
                .expect("import arguments"),
            ),
            other => panic!("unsupported V6 test command {other:?}"),
        };
        let universal_intent =
            UniversalIntent::new(None, variant, "operation.status.v6", participants)
                .expect("V6 universal intent");
        (
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
        )
    }

    fn v7_waveform_edit_inputs() -> (
        OperationId,
        OperationIntent,
        UniversalIntent,
        WaveformEditIntentArgs,
        DurableCapacityPlan,
    ) {
        let (operation_id, intent, copy_universal, copy_args, capacity_plan) = v5_copy_inputs();
        let participants = copy_universal.participants().clone();
        let universal_intent = UniversalIntent::new(
            None,
            UniversalCommandKind::DestructiveEdit,
            "operation.status.waveform-edit",
            participants,
        )
        .expect("waveform edit universal intent");
        let revision = WaveformFileRevisionObservation::new(
            "file-identity",
            "change-marker",
            1024,
            Some(12_000_000_000),
        )
        .expect("waveform file revision");
        let source_spec = WaveformSourceSpec::new(2, 48_000, 16, WaveformSampleFormat::Integer)
            .expect("waveform source spec");
        let range = WaveformEditFrameRange::new(2, 8).expect("waveform edit range");
        let operation = WaveformEditOperationArgs::GainAndFades(
            WaveformEditSettingsArgs::new(
                FiniteF32Bits::new(-0.0).expect("signed zero gain"),
                None,
                None,
            )
            .expect("waveform edit settings"),
        );
        let edit_args = WaveformEditIntentArgs::new(
            copy_args.source().clone(),
            revision,
            source_spec,
            10,
            range,
            operation,
            WaveformOutputSpec::float32(2, 48_000),
        )
        .expect("waveform edit arguments");
        (
            operation_id,
            intent,
            universal_intent,
            edit_args,
            capacity_plan,
        )
    }

    fn v8_namespace_inputs(
        variant: &str,
    ) -> (
        OperationId,
        OperationIntent,
        UniversalIntent,
        NamespaceCommandArgs,
        DurableCapacityPlan,
    ) {
        let (operation_id, intent, copy_universal, copy_args, capacity_plan) = v5_copy_inputs();
        let participants = copy_universal.participants().clone();
        let source_participant = copy_args.source().participant_id();
        let source_id = SourceId::from_string("source-a");
        let source = NamespaceEntryEndpoint::new(
            source_participant,
            source_id.clone(),
            StoredRelativePath::new("in.wav").expect("namespace source path"),
            NamespaceEntryExpectation::RegularFile(
                NamespaceFileExpectation::new(7, "file-identity", 10, Some(-2))
                    .expect("namespace file expectation"),
            ),
        )
        .expect("namespace source endpoint");
        let root_parent = || {
            NamespaceDirectoryEndpoint::new(
                source_participant,
                source_id.clone(),
                NamespaceDirectoryLocator::Root,
                NamespaceDirectoryExpectation::new(7, "root-a", Some(-1))
                    .expect("namespace root expectation"),
            )
            .expect("namespace root parent")
        };
        let command_args = match variant {
            "Rename" => NamespaceCommandArgs::Rename(
                NamespaceRenameIntentArgs::new(
                    source,
                    NamespaceTarget::new(
                        root_parent(),
                        NamespaceLeafName::new("out.wav").expect("rename leaf"),
                    )
                    .expect("rename target"),
                )
                .expect("rename args"),
            ),
            "Move" => NamespaceCommandArgs::Move(
                NamespaceMoveIntentArgs::new(
                    source,
                    NamespaceDirectoryEndpoint::new(
                        source_participant,
                        source_id.clone(),
                        NamespaceDirectoryLocator::child(
                            StoredRelativePath::new("archive").expect("archive path"),
                        )
                        .expect("archive locator"),
                        NamespaceDirectoryExpectation::new(7, "archive", None)
                            .expect("archive expectation"),
                    )
                    .expect("archive parent"),
                )
                .expect("move args"),
            ),
            "CreateFolder" => NamespaceCommandArgs::CreateFolder(
                CreateFolderIntentArgs::new(
                    root_parent(),
                    NamespaceLeafName::new("new-folder").expect("folder leaf"),
                )
                .expect("create-folder args"),
            ),
            other => panic!("unsupported namespace test command {other}"),
        };
        let universal_intent = UniversalIntent::new(
            None,
            command_args.command_kind(),
            "operation.status.namespace",
            participants,
        )
        .expect("namespace universal intent");
        (
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
        )
    }

    fn v9_render_inputs(
        variant: &str,
    ) -> (
        OperationId,
        OperationIntent,
        UniversalIntent,
        RenderCommandArgs,
        DurableCapacityPlan,
    ) {
        let (operation_id, intent, copy_universal, copy_args, capacity_plan) = v5_copy_inputs();
        let participants = copy_universal.participants().clone();
        let source_spec = WaveformSourceSpec::new(2, 48_000, 8, WaveformSampleFormat::Integer)
            .expect("render source spec");
        let revision = WaveformFileRevisionObservation::new(
            "render-file-identity",
            "render-change-marker",
            1024,
            Some(12_000_000_000),
        )
        .expect("render file revision");
        let command_args = match variant {
            "Normalize" => RenderCommandArgs::Normalize(
                WaveformNormalizeIntentArgs::new(
                    copy_args.source().clone(),
                    revision.clone(),
                    source_spec,
                    10,
                    WaveformOutputSpec::float32(2, 48_000),
                )
                .expect("normalize arguments"),
            ),
            "DuplicateDouble" => RenderCommandArgs::DuplicateDouble(
                WaveformDuplicateDoubleIntentArgs::new(
                    copy_args.source().clone(),
                    copy_args.destination().clone(),
                    revision,
                    source_spec,
                    10,
                    WaveformRenderOutputSpec::from_source(source_spec),
                    CopyCollisionPolicy::NoReplace,
                )
                .expect("duplicate-double arguments"),
            ),
            other => panic!("unsupported V9 render test command {other}"),
        };
        let universal_intent = UniversalIntent::new(
            None,
            command_args.command_kind(),
            "operation.status.render",
            participants,
        )
        .expect("render universal intent");
        (
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
        )
    }

    fn v10_trash_inputs() -> (
        OperationId,
        OperationIntent,
        UniversalIntent,
        TrashIntentArgs,
        DurableCapacityPlan,
    ) {
        let (operation_id, intent, copy_universal, copy_args, capacity_plan) = v5_copy_inputs();
        let participants = copy_universal.participants().clone();
        let source_participant = copy_args.source().participant_id();
        let filesystem_participant = participants
            .descriptors()
            .iter()
            .find(|descriptor| descriptor.kind() == ParticipantKind::Filesystem)
            .expect("filesystem descriptor")
            .id();
        let source_id = SourceId::from_string("source-a");
        let source = NamespaceEntryEndpoint::new(
            source_participant,
            source_id,
            StoredRelativePath::new("folder/item.wav").expect("trash source path"),
            NamespaceEntryExpectation::RegularFile(
                NamespaceFileExpectation::new(7, "file-identity", 10, Some(11))
                    .expect("trash file expectation"),
            ),
        )
        .expect("trash source endpoint");
        let command_args = TrashIntentArgs::new(
            source,
            ExternalRootBindingId::new(),
            filesystem_participant,
            "trash-root",
        )
        .expect("trash arguments");
        let universal_intent = UniversalIntent::new(
            None,
            UniversalCommandKind::Trash,
            "operation.status.trash",
            participants,
        )
        .expect("trash universal intent");
        (
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
        )
    }

    fn v11_retained_restore_inputs() -> (
        OperationId,
        OperationIntent,
        UniversalIntent,
        RetainedResolutionCommandArgs,
        DurableCapacityPlan,
    ) {
        let (operation_id, intent, copy_universal, copy_args, capacity_plan) = v5_copy_inputs();
        let participants = copy_universal.participants().clone();
        let source = NamespaceEntryEndpoint::new(
            copy_args.source().participant_id(),
            SourceId::from_string("source-a"),
            StoredRelativePath::new(".yield_delete_staging/folder/retained").expect("staged path"),
            NamespaceEntryExpectation::Directory(
                NamespaceDirectoryExpectation::new(7, "staged-directory", None)
                    .expect("staged expectation"),
            ),
        )
        .expect("staged endpoint");
        let observation = RetainedFolderObservation::new(
            StoredRelativePath::new("folder/retained").expect("original path"),
            source,
            StoredRelativePath::new("folder/retained").expect("staged relative"),
            RetainedDeleteId::new(Uuid::from_u128(11)).expect("retained ID"),
            RetainedResolutionStage::Deleted,
            RetainedRecoveryRootKind::LegacyDeleteStaging,
            fingerprint_selected_entry_json(
                br#"{"id":"retained","restore_stamp":null,"stage":"deleted"}"#,
            )
            .expect("entry fingerprint"),
        )
        .expect("retained observation");
        let command_args = RetainedResolutionCommandArgs::Restore(
            RetainedRestoreIntentArgs::new(observation).expect("restore arguments"),
        );
        let universal_intent = UniversalIntent::new(
            None,
            command_args.command_kind(),
            "operation.status.retained-restore",
            participants,
        )
        .expect("retained universal intent");
        (
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
        )
    }

    fn v12_selection_normalize_inputs() -> (
        OperationId,
        OperationIntent,
        UniversalIntent,
        SelectionNormalizeIntentArgs,
        DurableCapacityPlan,
    ) {
        let (operation_id, intent, copy_universal, copy_args, capacity_plan) = v5_copy_inputs();
        let participants = copy_universal.participants().clone();
        let source_spec = WaveformSourceSpec::new(2, 48_000, 16, WaveformSampleFormat::Integer)
            .expect("selection source spec");
        let command_args = SelectionNormalizeIntentArgs::new(
            copy_args.source().clone(),
            WaveformFileRevisionObservation::new(
                "selection-file-identity",
                "selection-change-marker",
                1024,
                Some(-12_000_000_000),
            )
            .expect("selection file revision"),
            source_spec,
            10,
            WaveformEditFrameRange::new(2, 8).expect("selection range"),
            2,
            WaveformOutputSpec::float32(2, 48_000),
        )
        .expect("selection Normalize arguments");
        let universal_intent = UniversalIntent::new(
            None,
            UniversalCommandKind::DestructiveEdit,
            "operation.status.selection-normalize",
            participants,
        )
        .expect("selection universal intent");
        (
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
        )
    }

    fn v13_selection_output_inputs() -> (
        OperationId,
        OperationIntent,
        UniversalIntent,
        SelectionOutputCommandArgs,
        DurableCapacityPlan,
    ) {
        let (operation_id, intent, copy_universal, copy_args, capacity_plan) = v5_copy_inputs();
        let participants = copy_universal.participants().clone();
        let source_spec = WaveformSourceSpec::new(2, 48_000, 16, WaveformSampleFormat::Integer)
            .expect("selection output source spec");
        let recipe = SelectionOutputRecipe::RawWav(
            RawWavExtractionRecipe::new(
                16,
                FiniteF32Bits::new(1.0).expect("selection output gain"),
                2,
            )
            .expect("selection output recipe"),
        );
        let extraction = SelectionExtractIntentArgs::new(
            copy_args.source().clone(),
            7,
            WaveformFileRevisionObservation::new(
                "selection-file-identity",
                "selection-change-marker",
                1024,
                Some(-12_000_000_000),
            )
            .expect("selection output file revision"),
            source_spec,
            10,
            WaveformEditFrameRange::new(2, 8).expect("selection output range"),
            recipe,
            SourcePlaybackType::OneShot,
            copy_args.destination().clone(),
            CopyCollisionPolicy::NoReplace,
        )
        .expect("selection output extraction");
        let command_args = SelectionOutputCommandArgs::ExtractAndTrim(
            ExtractAndTrimIntentArgs::new(extraction, WaveformOutputSpec::float32(2, 48_000))
                .expect("ExtractAndTrim arguments"),
        );
        let universal_intent = UniversalIntent::new(
            None,
            UniversalCommandKind::DestructiveEdit,
            "operation.status.extract-and-trim",
            participants,
        )
        .expect("selection output universal intent");
        (
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
        )
    }

    fn v14_history_inputs(
        direction: HistoryDirection,
    ) -> (
        OperationId,
        OperationIntent,
        UniversalIntent,
        HistoryUndoRedoIntentArgs,
        DurableCapacityPlan,
    ) {
        let (operation_id, mut intent, copy_universal, copy_args, capacity_plan) = v5_copy_inputs();
        intent.kind = OperationKind::FileHistory;
        let participants = copy_universal.participants().clone();
        let filesystem_participant_id = participants
            .descriptors()
            .iter()
            .find(|descriptor| descriptor.kind() == ParticipantKind::Filesystem)
            .expect("filesystem descriptor")
            .id();
        let revision = |identity: &str| {
            WaveformFileRevisionObservation::new(identity, "change", 10, Some(11))
                .expect("history revision")
        };
        let before =
            HistoryRecoveryLeafObservation::new(revision("before")).expect("history before backup");
        let after =
            HistoryRecoveryLeafObservation::new(revision("after")).expect("history after backup");
        let backups = match direction {
            HistoryDirection::Undo => HistoryRecoveryBackupObservation::new(
                filesystem_participant_id,
                "recovery-root",
                uuid::Uuid::from_u128(20),
                "edit-directory",
                Some(before),
                None,
                None,
            )
            .expect("history Undo backups"),
            HistoryDirection::Redo => HistoryRecoveryBackupObservation::new(
                filesystem_participant_id,
                "recovery-root",
                uuid::Uuid::from_u128(20),
                "edit-directory",
                None,
                Some(after),
                None,
            )
            .expect("history Redo backups"),
        };
        let restore = HistoryWaveformRestore::new(
            copy_args.source().clone(),
            7,
            HistoryRestoreTargetExpectation::Existing(revision("target")),
            Some(String::from("before-content")),
            backups,
            None,
        )
        .expect("history waveform restore");
        let command_args = HistoryUndoRedoIntentArgs::new(
            HistoryScopeId::from_uuid(uuid::Uuid::from_u128(30)),
            4,
            5,
            Some(6),
            direction,
            vec![HistoryAction::WaveformRestore(restore)],
        )
        .expect("history arguments");
        let universal_intent = UniversalIntent::new(
            None,
            direction.command_kind(),
            "operation.status.history",
            participants,
        )
        .expect("history universal intent");
        (
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
        )
    }

    fn v15_external_inputs() -> (
        OperationId,
        OperationIntent,
        UniversalIntent,
        ExternalReconciliationIntentArgs,
        DurableCapacityPlan,
    ) {
        let (operation_id, mut intent, copy_universal, copy_args, capacity_plan) = v5_copy_inputs();
        intent.actor = OperationActor::ExternalFs;
        intent.kind = OperationKind::ExternalReconciliation;
        let participants = copy_universal.participants().clone();
        let source_participant_id = copy_args.source().participant_id();
        let filesystem_participant_id = participants
            .descriptors()
            .iter()
            .find(|descriptor| descriptor.kind() == ParticipantKind::Filesystem)
            .expect("filesystem descriptor")
            .id();
        let provenance = ExternalRawObservationProvenance::new(
            SourceId::from_string("source-a"),
            Some(vec![1, 2, 3]),
            Some(vec![4, 5]),
            12,
            ExternalCaptureBoundary::new(50, Some(10), None).expect("capture boundary"),
        )
        .expect("external provenance");
        let marker = ExternalRawObservation::new(
            ExternalRawEventKind::Overflow,
            vec![
                crate::operation_journal::ExternalRawObservedPath::new(
                    ExternalNativeString::unix_bytes(b"changed.wav".to_vec()).expect("native path"),
                    crate::operation_journal::ExternalRawPathRole::Subject,
                    None,
                )
                .expect("path observation"),
            ],
            ExternalRawObservationMetadata::new()
                .with_cursor(vec![9, 8])
                .with_event_id(44),
            1 << 15,
        )
        .expect("external marker");
        let limits = ExternalRawObservationLimits::new(64, 4096, 4096).expect("raw limits");
        let raw = ExternalRawObservationEnvelope::new(
            provenance,
            limits,
            ExternalRawNativeAccounting::new(1, 11, 11),
            vec![marker],
        )
        .expect("external raw envelope");
        let command_args = ExternalReconciliationIntentArgs::new(
            EvidenceBatchId::from_uuid(Uuid::from_u128(55)),
            source_participant_id,
            filesystem_participant_id,
            7,
            raw,
        )
        .expect("external command arguments");
        let universal_intent = UniversalIntent::new(
            None,
            UniversalCommandKind::ExternalReconciliation,
            "operation.status.external-reconciliation",
            participants,
        )
        .expect("external universal intent");
        (
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
        )
    }

    fn external_universal_intent_record(operation_id: OperationId) -> StoredOperationRecord {
        let mut value: serde_json::Value = serde_json::from_slice(
            &encode(&universal_intent_record(operation_id)).expect("v3 bytes"),
        )
        .expect("parse v3 fixture");
        value["intent"]["actor"] = serde_json::Value::from("ExternalFs");
        value["intent"]["kind"] = serde_json::Value::from("ExternalReconciliation");
        value["universal_intent"]["command_kind"] =
            serde_json::Value::from("ExternalReconciliation");
        decode(&serde_json::to_vec(&value).expect("serialize external fixture"))
            .expect("decode external intent fixture")
    }

    fn database_path(owner: &WritableProfileGuard) -> PathBuf {
        owner
            .profile_root()
            .join(JOURNAL_DATABASE_ROOT_NAME)
            .join(JOURNAL_DATABASE_FILE_NAME)
    }

    fn write_v1_database(
        owner: &WritableProfileGuard,
        record: &StoredOperationRecord,
        sequence: i64,
    ) -> (PathBuf, Vec<u8>) {
        let path = database_path(owner);
        fs::create_dir_all(path.parent().expect("journal directory")).expect("journal directory");
        let encoded = encode(record).expect("encode legacy fixture");
        let connection = Connection::open(&path).expect("open legacy fixture");
        connection
            .execute_batch(&format!(
                "PRAGMA application_id = {JOURNAL_APPLICATION_ID};
                 PRAGMA user_version = {LEGACY_JOURNAL_SCHEMA_VERSION};
                 {CREATE_JOURNAL_TABLE};"
            ))
            .expect("create v1 journal schema");
        connection
            .execute(
                "INSERT INTO operation_journal (operation_id, sequence, record)
                 VALUES (?1, ?2, ?3)",
                params![record.operation_id.to_string(), sequence, &encoded],
            )
            .expect("insert v1 journal row");
        drop(connection);
        (path, encoded)
    }

    fn write_v2_database(
        owner: &WritableProfileGuard,
        record: &StoredOperationRecord,
        sequence: i64,
    ) -> (PathBuf, Vec<u8>) {
        let path = database_path(owner);
        fs::create_dir_all(path.parent().expect("journal directory")).expect("journal directory");
        let encoded = encode(record).expect("encode previous-schema fixture");
        let connection = Connection::open(&path).expect("open previous-schema fixture");
        connection
            .execute_batch(&format!(
                "PRAGMA application_id = {JOURNAL_APPLICATION_ID};
                 PRAGMA user_version = {PREVIOUS_JOURNAL_SCHEMA_VERSION};
                 {CREATE_JOURNAL_TABLE};
                 {CREATE_FULL_PROGRESS_TABLE};"
            ))
            .expect("create v2 journal schema");
        connection
            .execute(
                "INSERT INTO operation_journal (operation_id, sequence, record)
                 VALUES (?1, ?2, ?3)",
                params![record.operation_id.to_string(), sequence, &encoded],
            )
            .expect("insert v2 journal row");
        drop(connection);
        (path, encoded)
    }

    fn write_v3_database(
        owner: &WritableProfileGuard,
        record: &StoredOperationRecord,
        sequence: i64,
    ) -> (PathBuf, Vec<u8>) {
        let path = database_path(owner);
        fs::create_dir_all(path.parent().expect("journal directory")).expect("journal directory");
        let encoded = encode(record).expect("encode recovery-schema fixture");
        let connection = Connection::open(&path).expect("open recovery-schema fixture");
        connection
            .execute_batch(&format!(
                "PRAGMA application_id = {JOURNAL_APPLICATION_ID};
                 PRAGMA user_version = {RECOVERY_RESERVE_SCHEMA_VERSION};
                 {CREATE_JOURNAL_TABLE};
                 {CREATE_FULL_PROGRESS_TABLE};
                 {charge_table};
                 {transition_table};",
                charge_table = recovery_reserve::CREATE_CHARGE_TABLE,
                transition_table = recovery_reserve::CREATE_TRANSITION_TABLE,
            ))
            .expect("create v3 journal schema");
        connection
            .execute(
                "INSERT INTO operation_journal (operation_id, sequence, record)
                 VALUES (?1, ?2, ?3)",
                params![record.operation_id.to_string(), sequence, &encoded],
            )
            .expect("insert v3 journal row");
        drop(connection);
        (path, encoded)
    }

    fn write_v4_database(
        owner: &WritableProfileGuard,
        record: &StoredOperationRecord,
        sequence: i64,
    ) -> (PathBuf, Vec<u8>) {
        let path = database_path(owner);
        fs::create_dir_all(path.parent().expect("journal directory")).expect("journal directory");
        let encoded = encode(record).expect("encode binding-schema fixture");
        let connection = Connection::open(&path).expect("open binding-schema fixture");
        connection
            .execute_batch(&format!(
                "PRAGMA application_id = {JOURNAL_APPLICATION_ID};
                 PRAGMA user_version = {RECOVERY_RESERVE_BINDING_SCHEMA_VERSION};
                 {CREATE_JOURNAL_TABLE};
                 {CREATE_FULL_PROGRESS_TABLE};
                 {charge_table};
                 {binding_table};
                 {transition_table};",
                charge_table = recovery_reserve::CREATE_CHARGE_TABLE,
                binding_table = recovery_reserve_binding::CREATE_TABLE,
                transition_table = recovery_reserve::CREATE_TRANSITION_TABLE,
            ))
            .expect("create v4 journal schema");
        connection
            .execute(
                "INSERT INTO operation_journal (operation_id, sequence, record)
                 VALUES (?1, ?2, ?3)",
                params![record.operation_id.to_string(), sequence, &encoded],
            )
            .expect("insert v4 journal row");
        let (_, progressed) = full_progress_store_records();
        let progressed_encoded = crate::operation_journal::full_progress_codec::encode(&progressed)
            .expect("encode v4 full-progress fixture");
        let progressed_operation_id = progressed.snapshot().intent().operation_id();
        connection
            .execute(
                "INSERT INTO operation_journal_full_progress (operation_id, sequence, record)
                 VALUES (?1, ?2, ?3)",
                params![
                    progressed_operation_id.to_string(),
                    17_i64,
                    &progressed_encoded
                ],
            )
            .expect("insert v4 full-progress row");
        connection
            .execute(
                &format!(
                    "INSERT INTO {table}
                     (volume_device, profile_identity, root_identity, file_identity,
                      allocation_unit, protected_floor_bytes, namespace_version,
                      namespace_name, owner_generation)
                     VALUES ('7', 'fixture-profile', 'fixture-root', 'fixture-file',
                             4096, ?1, 1, 'recovery-reserve.bin', 1)",
                    table = recovery_reserve_binding::TABLE_NAME
                ),
                params![i64::try_from(PROTECTED_FREE_SPACE_FLOOR).expect("fixture floor")],
            )
            .expect("insert v4 reserve binding");
        let charge_id = Uuid::from_u128(40_001);
        let transition_id = Uuid::from_u128(40_002);
        connection
            .execute(
                &format!(
                    "INSERT INTO {table}
                     (charge_id, volume_device, active_volume_device, operation_id, purpose,
                      budget_bytes, margin_bytes, precharge_capacity_bytes, remaining_bytes,
                      state, transition_sequence)
                     VALUES (?1, '7', '7', ?2, 'OperationRecovery', 10, 1, 0, 10,
                             'Charged', 1)",
                    table = recovery_reserve::CHARGE_TABLE_NAME
                ),
                params![charge_id.to_string(), record.operation_id.to_string()],
            )
            .expect("insert v4 recovery charge");
        connection
            .execute(
                &format!(
                    "INSERT INTO {table}
                     (charge_id, transition_id, transition_sequence, kind, amount_bytes,
                      remaining_bytes, state)
                     VALUES (?1, ?2, 1, 'Charged', 10, 10, 'Charged')",
                    table = recovery_reserve::TRANSITION_TABLE_NAME
                ),
                params![charge_id.to_string(), transition_id.to_string()],
            )
            .expect("insert v4 recovery transition");
        drop(connection);
        (path, encoded)
    }

    fn write_v5_database(
        owner: &WritableProfileGuard,
        record: &StoredOperationRecord,
        sequence: i64,
    ) -> (PathBuf, Vec<u8>) {
        let (path, encoded) = write_v4_database(owner, record, sequence);
        let connection = Connection::open(&path).expect("open ordinary-claim schema fixture");
        connection
            .execute_batch(&format!(
                "{ordinary_claim_table}; PRAGMA user_version = {ORDINARY_CLAIM_SCHEMA_VERSION};",
                ordinary_claim_table = ordinary_claims::CREATE_TABLE,
            ))
            .expect("create v5 ordinary-claim schema");
        drop(connection);
        (path, encoded)
    }

    fn insert_ordinary_claim(
        store: &OperationJournalStore,
        claim_id: Uuid,
        operation_id: Uuid,
        volume_device: &str,
        allocation_class: &str,
        allocated_bytes: i64,
    ) -> rusqlite::Result<usize> {
        store.connection.execute(
            &format!(
                "INSERT INTO {table}
                 (claim_id, operation_id, volume_device, allocation_class, allocated_bytes)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                table = ordinary_claims::TABLE_NAME
            ),
            params![
                claim_id.to_string(),
                operation_id.to_string(),
                volume_device,
                allocation_class,
                allocated_bytes
            ],
        )
    }

    #[test]
    fn typed_insert_outcome_acknowledges_exact_readback() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let record = intent_record(OperationId::new());

        let outcome = store.insert_intent_with_outcome(&record);
        let receipt = outcome.receipt().expect("fully acknowledged insert");
        assert_eq!(receipt.operation_id(), record.operation_id);
        assert_eq!(receipt.sequence(), 1);
        assert_eq!(
            store
                .get(record.operation_id)
                .expect("read acknowledged row"),
            Some(record)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn recovery_context_requires_live_bindings_and_reconstitutes_with_fresh_proof() {
        let fixture = fixture();
        let reserve_root = BoundDatabaseRoot::for_profile_child(
            &fixture.owner,
            Path::new("context-recovery-reserve"),
        )
        .expect("reserve root");
        let reserve_path = fixture
            .owner
            .profile_root()
            .join("context-recovery-reserve");
        let reserve_file_path = reserve_path.join("recovery-reserve.bin");
        let reserve = super::super::capacity_reserve_namespace::create_owned_reserve(
            &reserve_root,
            &fixture.owner,
        )
        .expect("allocate one protected reserve");
        let expected_observation = reserve.observation().clone();
        let volume = expected_observation.volume().clone();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let operation_id = OperationId::new();
        let receipt = store
            .insert_intent_with_outcome(&intent_record(operation_id))
            .receipt()
            .expect("durable operation intent");
        assert_eq!(receipt.operation_id(), operation_id);

        let make_registry = || {
            let temp_root = fs::canonicalize(std::env::temp_dir()).expect("canonical temp root");
            let directory = tempfile::tempdir_in(temp_root).expect("ownership registry directory");
            let registry = directory.path().join("volume-ownership");
            fs::create_dir(&registry).expect("ownership registry");
            (directory, registry)
        };
        let descriptor_path = fixture._base.path().join("context-owner-descriptor");
        fs::write(&descriptor_path, b"descriptor").expect("owner descriptor");
        let descriptor = fs::File::open(&descriptor_path).expect("open owner descriptor");

        let (wrong_profile_root, wrong_profile_registry) = make_registry();
        let wrong_profile = super::super::capacity_volume_owner::acquire_fixture_for_test(
            &descriptor,
            &wrong_profile_registry,
            volume.clone(),
            "different-profile",
        )
        .expect("wrong-profile fixture owner");
        let wrong_profile_error = store
            .bind_recovery_reserve(wrong_profile, reserve)
            .err()
            .expect("profile mismatch must reject before ledger transition");
        assert!(matches!(
            wrong_profile_error,
            RecoveryReserveContextError::ProfileIdentityMismatch
        ));
        let charge_count: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM operation_journal_recovery_reserve",
                [],
                |row| row.get(0),
            )
            .expect("count untouched charges");
        assert_eq!(charge_count, 0);
        drop(wrong_profile_root);

        let reserve = super::super::capacity_reserve_namespace::reopen_owned_reserve(
            &reserve_root,
            &fixture.owner,
            &expected_observation,
        )
        .expect("reopen reserve after profile mismatch");
        let (wrong_volume_root, wrong_volume_registry) = make_registry();
        let wrong_volume = super::super::capacity_volume_owner::acquire_fixture_for_test(
            &descriptor,
            &wrong_volume_registry,
            VolumeIdentity {
                device: volume.device.checked_add(1).expect("test volume device"),
            },
            fixture.owner.profile_root_identity(),
        )
        .expect("wrong-volume fixture owner");
        let wrong_volume_error = store
            .bind_recovery_reserve(wrong_volume, reserve)
            .err()
            .expect("volume mismatch must reject before ledger transition");
        assert!(matches!(
            wrong_volume_error,
            RecoveryReserveContextError::VolumeIdentityMismatch
        ));
        drop(wrong_volume_root);

        let reserve = super::super::capacity_reserve_namespace::reopen_owned_reserve(
            &reserve_root,
            &fixture.owner,
            &expected_observation,
        )
        .expect("reopen reserve after volume mismatch");
        let (owner_registry_root, owner_registry) = make_registry();
        let volume_owner = super::super::capacity_volume_owner::acquire_fixture_for_test(
            &descriptor,
            &owner_registry,
            volume.clone(),
            fixture.owner.profile_root_identity(),
        )
        .expect("matching volume owner");
        let mut context = store
            .bind_recovery_reserve(volume_owner, reserve)
            .expect("bind live recovery context");
        let charge_id = Uuid::new_v4();
        let request = super::recovery_reserve::ChargeRequest {
            charge_id,
            operation_id,
            volume: volume.clone(),
            purpose: super::recovery_reserve::RecoveryReservePurpose::OperationRecovery,
            budget_bytes: 32 * 1024,
            margin_bytes: 4096,
            precharge_capacity_bytes: u64::MAX,
        };
        let charged = context.begin_charge(&request).expect("durable charge");
        assert_eq!(
            charged.state,
            super::recovery_reserve::RecoveryReserveState::Charged
        );
        assert_ne!(
            charged.precharge_capacity_bytes, request.precharge_capacity_bytes,
            "the persisted observation must come from the live descriptor"
        );

        let charge_b = Uuid::new_v4();
        let volume_b = VolumeIdentity {
            device: volume.device.checked_add(1).expect("second test volume"),
        };
        let request_b = super::recovery_reserve::ChargeRequest {
            charge_id: charge_b,
            operation_id,
            volume: volume_b,
            purpose: super::recovery_reserve::RecoveryReservePurpose::OperationRecovery,
            budget_bytes: 1024,
            margin_bytes: 16,
            precharge_capacity_bytes: 0,
        };
        super::recovery_reserve::begin_charge(&store.connection, &request_b)
            .expect("independent second-volume charge");
        let charge_b_row = || {
            store.connection.query_row(
                "SELECT volume_device, active_volume_device, remaining_bytes, state,
                        transition_sequence
                 FROM operation_journal_recovery_reserve WHERE charge_id = ?1",
                params![charge_b.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
        };
        let charge_b_transitions = || {
            let mut statement = store
                .connection
                .prepare(
                    "SELECT transition_id, transition_sequence, kind, amount_bytes,
                            remaining_bytes, state
                     FROM operation_journal_recovery_reserve_transition
                     WHERE charge_id = ?1 ORDER BY transition_sequence",
                )
                .expect("prepare second-volume transitions");
            statement
                .query_map(params![charge_b.to_string()], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                })
                .expect("read second-volume transitions")
                .collect::<Result<Vec<_>, _>>()
                .expect("decode second-volume transitions")
        };
        let charge_b_before = charge_b_row().expect("read second-volume charge");
        let charge_b_transitions_before = charge_b_transitions();
        assert!(matches!(
            context.spend(charge_b, Uuid::new_v4(), 1),
            Err(RecoveryReserveContextError::Ledger(
                super::recovery_reserve::LedgerError::VolumeConflict { .. }
            ))
        ));
        assert!(matches!(
            context.record_release_observation(charge_b, Uuid::new_v4(), 1),
            Err(RecoveryReserveContextError::Ledger(
                super::recovery_reserve::LedgerError::VolumeConflict { .. }
            ))
        ));
        assert!(matches!(
            context.reconstitute(charge_b, Uuid::new_v4()),
            Err(RecoveryReserveContextError::Ledger(
                super::recovery_reserve::LedgerError::VolumeConflict { .. }
            ))
        ));
        assert_eq!(
            charge_b_row().expect("read unchanged second-volume charge"),
            charge_b_before
        );
        assert_eq!(charge_b_transitions(), charge_b_transitions_before);

        let allocation_unit = expected_observation.allocation_unit();
        let rounded_spend = crate::durable_capacity::round_up_allocation(512, allocation_unit)
            .expect("round physical spend");
        let floor = PROTECTED_FREE_SPACE_FLOOR;
        let initial_length = fs::metadata(&reserve_file_path)
            .expect("read initial reserve metadata")
            .len();
        assert_eq!(initial_length, floor);

        let overspend = context
            .spend_physical(charge_id, Uuid::new_v4(), 64 * 1024)
            .expect_err("overspend must reject before physical mutation");
        assert!(matches!(
            overspend,
            RecoveryReserveContextError::Ledger(
                super::recovery_reserve::LedgerError::BudgetExceeded { .. }
            )
        ));
        assert_eq!(
            fs::metadata(&reserve_file_path)
                .expect("read reserve after overspend")
                .len(),
            initial_length
        );

        let first_spend_id = Uuid::new_v4();
        let before_truncate = context
            .spend_physical_with_test_fault(
                charge_id,
                first_spend_id,
                512,
                super::super::capacity_reserve::TestReserveMutationFault::BeforeTruncate,
            )
            .expect_err("failure before truncate must retain durable spend");
        assert!(matches!(
            before_truncate,
            RecoveryReserveContextError::Reserve(_)
        ));
        let after_first_failure = super::recovery_reserve::charge_snapshot_for_volume(
            &store.connection,
            &volume,
            charge_id,
        )
        .expect("read durable first spend");
        assert_eq!(
            after_first_failure.state,
            super::recovery_reserve::RecoveryReserveState::Spent
        );
        assert_eq!(
            after_first_failure.remaining_bytes,
            32 * 1024 - rounded_spend
        );
        assert_eq!(
            fs::metadata(&reserve_file_path)
                .expect("read reserve after pre-truncate fault")
                .len(),
            initial_length
        );

        let first_retry = context
            .spend_physical(charge_id, first_spend_id, 512)
            .expect("retry first physical spend");
        assert_eq!(first_retry.transition_sequence, 2);
        assert_eq!(first_retry.remaining_bytes, 32 * 1024 - rounded_spend);
        assert_eq!(
            fs::metadata(&reserve_file_path)
                .expect("read reserve after first retry")
                .len(),
            floor - rounded_spend
        );

        let second_spend_id = Uuid::new_v4();
        let after_truncate = context
            .spend_physical_with_test_fault(
                charge_id,
                second_spend_id,
                512,
                super::super::capacity_reserve::TestReserveMutationFault::AfterTruncateBeforeSync,
            )
            .expect_err("failure after truncate must retain durable spend");
        assert!(matches!(
            after_truncate,
            RecoveryReserveContextError::Reserve(_)
        ));
        assert_eq!(
            fs::metadata(&reserve_file_path)
                .expect("read reserve after post-truncate fault")
                .len(),
            floor - 2 * rounded_spend
        );
        let second_retry = context
            .spend_physical(charge_id, second_spend_id, 512)
            .expect("retry second physical spend");
        assert_eq!(second_retry.transition_sequence, 3);
        assert_eq!(second_retry.remaining_bytes, 32 * 1024 - 2 * rounded_spend);
        let first_late_retry = context
            .spend_physical(charge_id, first_spend_id, 512)
            .expect("late retry must use current durable head");
        assert_eq!(first_late_retry.transition_sequence, 2);
        assert_eq!(
            fs::metadata(&reserve_file_path)
                .expect("read reserve after late retry")
                .len(),
            floor - 2 * rounded_spend
        );
        assert!(
            super::recovery_reserve::readiness(&store.connection)
                .expect("read recovery readiness")
                .is_some(),
            "Spent durable state must keep ordinary readiness blocked"
        );

        let displaced_root = fixture
            ._base
            .path()
            .join("context-recovery-reserve-displaced");
        fs::rename(&reserve_path, &displaced_root).expect("displace reserve root");
        fs::create_dir(&reserve_path).expect("install replacement reserve root");
        let stale_error = context
            .reconstitute_physical(charge_id, Uuid::new_v4())
            .expect_err("reconstitution must reject a replaced reserve root");
        assert!(matches!(
            stale_error,
            RecoveryReserveContextError::Reserve(_)
        ));
        let state_after_stale: String = store
            .connection
            .query_row(
                "SELECT state FROM operation_journal_recovery_reserve WHERE charge_id = ?1",
                params![charge_id.to_string()],
                |row| row.get(0),
            )
            .expect("read state after stale proof");
        assert_eq!(state_after_stale, "Spent");
        fs::remove_dir(&reserve_path).expect("remove replacement reserve root");
        fs::rename(&displaced_root, &reserve_path).expect("restore reserve root");

        let a_reconstitute_id = Uuid::new_v4();
        let reconstituted = context
            .reconstitute_physical(charge_id, a_reconstitute_id)
            .expect("fresh physical proof reconstitution");
        assert_eq!(
            reconstituted.state,
            super::recovery_reserve::RecoveryReserveState::Reconstituted
        );
        let restored_metadata = fs::metadata(&reserve_file_path).expect("read restored reserve");
        assert_eq!(restored_metadata.len(), floor);
        assert!(
            restored_metadata.blocks().checked_mul(512).unwrap_or(0) >= floor,
            "reconstitution must restore physical allocation"
        );

        let terminal_a = super::recovery_reserve::charge_snapshot_for_volume(
            &store.connection,
            &volume,
            charge_id,
        )
        .expect("read completed charge A");
        let terminal_length = restored_metadata.len();
        assert_eq!(
            context
                .spend_physical(charge_id, first_spend_id, 512)
                .expect("retry completed charge A spend")
                .state,
            super::recovery_reserve::RecoveryReserveState::Spent
        );
        assert_eq!(
            context
                .reconstitute_physical(charge_id, a_reconstitute_id)
                .expect("retry completed charge A reconstitution")
                .state,
            super::recovery_reserve::RecoveryReserveState::Reconstituted
        );
        assert!(matches!(
            context.spend_physical(charge_id, Uuid::new_v4(), 512),
            Err(RecoveryReserveContextError::Ledger(
                super::recovery_reserve::LedgerError::AlreadyReconstituted(_)
            ))
        ));
        assert!(matches!(
            context.reconstitute_physical(charge_id, Uuid::new_v4()),
            Err(RecoveryReserveContextError::Ledger(
                super::recovery_reserve::LedgerError::AlreadyReconstituted(_)
            ))
        ));
        assert_eq!(
            super::recovery_reserve::charge_snapshot_for_volume(
                &store.connection,
                &volume,
                charge_id,
            )
            .expect("read unchanged completed charge A")
            .state,
            terminal_a.state
        );
        assert_eq!(
            fs::metadata(&reserve_file_path)
                .expect("read reserve after completed charge A retries")
                .len(),
            terminal_length
        );

        let charge_c = Uuid::new_v4();
        let request_c = super::recovery_reserve::ChargeRequest {
            charge_id: charge_c,
            operation_id,
            volume: volume.clone(),
            purpose: super::recovery_reserve::RecoveryReservePurpose::OperationRecovery,
            budget_bytes: 16 * 1024,
            margin_bytes: 4096,
            precharge_capacity_bytes: u64::MAX,
        };
        context
            .begin_charge(&request_c)
            .expect("charge after prior reconstitution");
        let spend_c = Uuid::new_v4();
        context
            .spend_physical(charge_c, spend_c, 512)
            .expect("spend second recovery charge");
        let partial_length = fs::metadata(&reserve_file_path)
            .expect("read partial second reserve")
            .len();
        assert_eq!(partial_length, floor - rounded_spend);
        let charge_c_before_old_a_retry = super::recovery_reserve::charge_snapshot_for_volume(
            &store.connection,
            &volume,
            charge_c,
        )
        .expect("read active charge B");
        assert_eq!(
            context
                .spend_physical(charge_id, first_spend_id, 512)
                .expect("retry old charge A spend while B is active")
                .state,
            super::recovery_reserve::RecoveryReserveState::Spent
        );
        assert_eq!(
            context
                .reconstitute_physical(charge_id, a_reconstitute_id)
                .expect("retry old charge A reconstitution while B is active")
                .state,
            super::recovery_reserve::RecoveryReserveState::Reconstituted
        );
        assert!(matches!(
            context.spend_physical(charge_id, Uuid::new_v4(), 512),
            Err(RecoveryReserveContextError::Ledger(
                super::recovery_reserve::LedgerError::AlreadyReconstituted(_)
            ))
        ));
        assert!(matches!(
            context.reconstitute_physical(charge_id, Uuid::new_v4()),
            Err(RecoveryReserveContextError::Ledger(
                super::recovery_reserve::LedgerError::AlreadyReconstituted(_)
            ))
        ));
        assert_eq!(
            super::recovery_reserve::charge_snapshot_for_volume(
                &store.connection,
                &volume,
                charge_c,
            )
            .expect("read unchanged active charge B"),
            charge_c_before_old_a_retry
        );
        assert_eq!(
            fs::metadata(&reserve_file_path)
                .expect("read reserve after old charge A retries")
                .len(),
            partial_length
        );
        let allocator_failure = context
            .reconstitute_physical_with_test_fault(
                charge_c,
                Uuid::new_v4(),
                super::super::capacity_reserve::TestReserveMutationFault::Allocate,
            )
            .expect_err("allocator failure must leave durable spend blocked");
        assert!(matches!(
            allocator_failure,
            RecoveryReserveContextError::Reserve(_)
        ));
        let after_allocator_failure = super::recovery_reserve::charge_snapshot_for_volume(
            &store.connection,
            &volume,
            charge_c,
        )
        .expect("read second charge after allocator failure");
        assert_eq!(
            after_allocator_failure.state,
            super::recovery_reserve::RecoveryReserveState::Spent
        );
        assert_eq!(
            fs::metadata(&reserve_file_path)
                .expect("read reserve after allocator failure")
                .len(),
            partial_length
        );
        let sync_failure = context
            .reconstitute_physical_with_test_fault(
                charge_c,
                Uuid::new_v4(),
                super::super::capacity_reserve::TestReserveMutationFault::Sync,
            )
            .expect_err("sync failure must leave durable spend blocked");
        assert!(matches!(
            sync_failure,
            RecoveryReserveContextError::Reserve(_)
        ));
        assert_eq!(
            super::recovery_reserve::charge_snapshot_for_volume(
                &store.connection,
                &volume,
                charge_c,
            )
            .expect("read second charge after sync failure")
            .state,
            super::recovery_reserve::RecoveryReserveState::Spent
        );
        assert_eq!(
            fs::metadata(&reserve_file_path)
                .expect("read reserve after sync failure")
                .len(),
            floor
        );
        let second_reconstituted = context
            .reconstitute_physical(charge_c, Uuid::new_v4())
            .expect("retry physical reconstitution after sync failure");
        assert_eq!(
            second_reconstituted.state,
            super::recovery_reserve::RecoveryReserveState::Reconstituted
        );
        drop(context);
        drop(owner_registry_root);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn recovery_reserve_restart_reopens_full_and_partial_binding() {
        let fixture = fixture();
        let reserve_root = BoundDatabaseRoot::for_profile_child(
            &fixture.owner,
            Path::new("restart-recovery-reserve"),
        )
        .expect("restart reserve root");
        let reserve_path = fixture
            .owner
            .profile_root()
            .join("restart-recovery-reserve");
        let reserve_file_path = reserve_path.join("recovery-reserve.bin");
        let reserve = super::super::capacity_reserve_namespace::create_owned_reserve(
            &reserve_root,
            &fixture.owner,
        )
        .expect("allocate restart reserve");
        let expected = reserve.observation().clone();
        let volume = expected.volume().clone();
        let descriptor_path = fixture._base.path().join("restart-owner-descriptor");
        fs::write(&descriptor_path, b"descriptor").expect("write owner descriptor");
        let descriptor = fs::File::open(&descriptor_path).expect("open owner descriptor");
        let temp_root = fs::canonicalize(std::env::temp_dir()).expect("canonical temp root");
        let registry_root = tempfile::tempdir_in(temp_root).expect("ownership registry root");
        let registry_path = registry_root.path().join("volume-ownership");
        fs::create_dir(&registry_path).expect("create ownership registry");
        let store = OperationJournalStore::open(&fixture.owner).expect("open restart store");
        let operation_id = OperationId::new();
        store
            .insert_intent_with_outcome(&intent_record(operation_id))
            .receipt()
            .expect("persist restart operation");
        let owner = super::super::capacity_volume_owner::acquire_fixture_for_test(
            &descriptor,
            &registry_path,
            volume.clone(),
            fixture.owner.profile_root_identity(),
        )
        .expect("acquire initial volume owner");
        let context = store
            .bind_recovery_reserve(owner, reserve)
            .expect("bind initial reserve");
        let charge_id = Uuid::new_v4();
        context
            .begin_charge(&super::recovery_reserve::ChargeRequest {
                charge_id,
                operation_id,
                volume: volume.clone(),
                purpose: super::recovery_reserve::RecoveryReservePurpose::OperationRecovery,
                budget_bytes: 32 * 1024,
                margin_bytes: 4096,
                precharge_capacity_bytes: u64::MAX,
            })
            .expect("persist initial charged state");
        drop(context);
        drop(store);

        let store = OperationJournalStore::open(&fixture.owner).expect("reopen charged store");
        let owner = super::super::capacity_volume_owner::acquire_fixture_for_test(
            &descriptor,
            &registry_path,
            volume.clone(),
            fixture.owner.profile_root_identity(),
        )
        .expect("reacquire same owner after restart");
        let mut context = store
            .reopen_recovery_reserve(owner, &reserve_root, &fixture.owner)
            .expect("reopen full charged reserve");
        assert_eq!(
            fs::metadata(&reserve_file_path)
                .expect("read full reserve")
                .len(),
            PROTECTED_FREE_SPACE_FLOOR
        );
        let allocation_unit = expected.allocation_unit();
        let rounded_spend = crate::durable_capacity::round_up_allocation(512, allocation_unit)
            .expect("round restart spend");
        let spend_id = Uuid::new_v4();
        context
            .spend_physical(charge_id, spend_id, 512)
            .expect("spend after charged restart");
        let partial_length = fs::metadata(&reserve_file_path)
            .expect("read partial reserve")
            .len();
        assert_eq!(partial_length, PROTECTED_FREE_SPACE_FLOOR - rounded_spend);
        drop(context);
        drop(store);

        let store = OperationJournalStore::open(&fixture.owner).expect("reopen spent store");
        let owner = super::super::capacity_volume_owner::acquire_fixture_for_test(
            &descriptor,
            &registry_path,
            volume.clone(),
            fixture.owner.profile_root_identity(),
        )
        .expect("reacquire spent owner");
        let context = store
            .reopen_recovery_reserve(owner, &reserve_root, &fixture.owner)
            .expect("reopen partial spent reserve");
        assert_eq!(
            fs::metadata(&reserve_file_path)
                .expect("read reopened partial reserve")
                .len(),
            partial_length
        );

        // A crash that leaves the logical extent one byte below the durable minimum must retain
        // the file and reject restart before any physical or ledger mutation.
        let short_length = partial_length
            .checked_sub(1)
            .expect("nonzero partial extent");
        let short_file = fs::OpenOptions::new()
            .write(true)
            .open(&reserve_file_path)
            .expect("open reserve for short-extent fixture");
        short_file
            .set_len(short_length)
            .expect("truncate short-extent fixture");
        short_file.sync_all().expect("sync short-extent fixture");
        drop(context);
        drop(store);

        let store = OperationJournalStore::open(&fixture.owner).expect("reopen short store");
        let owner = super::super::capacity_volume_owner::acquire_fixture_for_test(
            &descriptor,
            &registry_path,
            volume.clone(),
            fixture.owner.profile_root_identity(),
        )
        .expect("reacquire short-extent owner");
        let short_error = store
            .reopen_recovery_reserve(owner, &reserve_root, &fixture.owner)
            .err()
            .expect("short extent must reject restart");
        assert!(matches!(
            short_error,
            RecoveryReserveContextError::Reserve(_)
        ));
        assert_eq!(
            fs::metadata(&reserve_file_path)
                .expect("read retained short extent")
                .len(),
            short_length
        );
        drop(store);

        // Restore the same allocation-unit block, then make the logical extent sparse. The
        // restart path must reject insufficient non-sparse allocation without replacing it.
        let sparse_file = fs::OpenOptions::new()
            .write(true)
            .open(&reserve_file_path)
            .expect("open reserve for sparse fixture");
        sparse_file
            .set_len(partial_length)
            .expect("restore partial logical extent");
        sparse_file
            .sync_all()
            .expect("sync restored partial extent");
        sparse_file
            .set_len(0)
            .expect("clear reserve for sparse fixture");
        sparse_file.sync_all().expect("sync sparse clear");
        sparse_file
            .set_len(partial_length)
            .expect("create sparse partial extent");
        sparse_file.sync_all().expect("sync sparse partial extent");
        drop(sparse_file);
        let sparse_allocation = fs::metadata(&reserve_file_path)
            .expect("read sparse allocation")
            .blocks()
            .checked_mul(512)
            .expect("sparse allocation arithmetic");
        assert!(
            sparse_allocation
                < crate::durable_capacity::round_up_allocation(partial_length, allocation_unit)
                    .expect("round sparse extent"),
            "fixture must have insufficient non-sparse allocation"
        );
        let store = OperationJournalStore::open(&fixture.owner).expect("reopen sparse store");
        let owner = super::super::capacity_volume_owner::acquire_fixture_for_test(
            &descriptor,
            &registry_path,
            volume.clone(),
            fixture.owner.profile_root_identity(),
        )
        .expect("reacquire sparse owner");
        let sparse_error = store
            .reopen_recovery_reserve(owner, &reserve_root, &fixture.owner)
            .err()
            .expect("sparse extent must reject restart");
        assert!(matches!(
            sparse_error,
            RecoveryReserveContextError::Reserve(_)
        ));
        assert_eq!(
            fs::metadata(&reserve_file_path)
                .expect("read retained sparse extent")
                .len(),
            partial_length
        );
        drop(store);

        // Rebuild the same retained descriptor's physical floor for the next crash-window case.
        let empty_file = fs::OpenOptions::new()
            .write(true)
            .open(&reserve_file_path)
            .expect("open sparse reserve for physical restore");
        empty_file.set_len(0).expect("clear sparse reserve");
        empty_file.sync_all().expect("sync cleared reserve");
        let restored_reserve =
            super::super::capacity_reserve::allocate_protected_reserve(empty_file)
                .expect("restore retained reserve floor");
        assert_eq!(
            restored_reserve.observation().length,
            PROTECTED_FREE_SPACE_FLOOR
        );
        drop(restored_reserve);

        let store = OperationJournalStore::open(&fixture.owner).expect("reopen restored store");
        let owner = super::super::capacity_volume_owner::acquire_fixture_for_test(
            &descriptor,
            &registry_path,
            volume.clone(),
            fixture.owner.profile_root_identity(),
        )
        .expect("reacquire restored owner");
        let mut context = store
            .reopen_recovery_reserve(owner, &reserve_root, &fixture.owner)
            .expect("reopen physically restored spent reserve");
        let before_failure = super::recovery_reserve::charge_snapshot_for_volume(
            &store.connection,
            &volume,
            charge_id,
        )
        .expect("read spent restart charge");
        let allocation_failure = context
            .reconstitute_physical_with_test_fault(
                charge_id,
                Uuid::new_v4(),
                super::super::capacity_reserve::TestReserveMutationFault::Allocate,
            )
            .expect_err("failed physical reconstitution remains blocked");
        assert!(matches!(
            allocation_failure,
            RecoveryReserveContextError::Reserve(_)
        ));
        assert_eq!(
            super::recovery_reserve::charge_snapshot_for_volume(
                &store.connection,
                &volume,
                charge_id,
            )
            .expect("read unchanged spent charge"),
            before_failure
        );
        assert_eq!(
            fs::metadata(&reserve_file_path)
                .expect("read reserve after failed reconstitution")
                .len(),
            PROTECTED_FREE_SPACE_FLOOR
        );

        // The physical restore above is intentionally left uncommitted. Dropping the context
        // models a crash before the durable Reconstituted transition; restart must retain the
        // Spent ledger head and then commit it only after a fresh proof.
        drop(context);
        drop(store);
        let store = OperationJournalStore::open(&fixture.owner).expect("reopen crash-window store");
        let owner = super::super::capacity_volume_owner::acquire_fixture_for_test(
            &descriptor,
            &registry_path,
            volume.clone(),
            fixture.owner.profile_root_identity(),
        )
        .expect("reacquire crash-window owner");
        let mut context = store
            .reopen_recovery_reserve(owner, &reserve_root, &fixture.owner)
            .expect("reopen physically restored spent reserve");
        assert_eq!(
            super::recovery_reserve::charge_snapshot_for_volume(
                &store.connection,
                &volume,
                charge_id,
            )
            .expect("read spent state before durable restore")
            .state,
            super::recovery_reserve::RecoveryReserveState::Spent
        );
        context
            .reconstitute_physical(charge_id, Uuid::new_v4())
            .expect("commit reconstitution after restart proof");
        assert_eq!(
            fs::metadata(&reserve_file_path)
                .expect("read restored reserve")
                .len(),
            PROTECTED_FREE_SPACE_FLOOR
        );
        drop(context);
        drop(store);

        let store =
            OperationJournalStore::open(&fixture.owner).expect("reopen reconstituted store");
        let owner = super::super::capacity_volume_owner::acquire_fixture_for_test(
            &descriptor,
            &registry_path,
            volume.clone(),
            fixture.owner.profile_root_identity(),
        )
        .expect("reacquire terminal owner");
        let context = store
            .reopen_recovery_reserve(owner, &reserve_root, &fixture.owner)
            .expect("repeat terminal reconciliation");
        assert_eq!(
            context.reserve.observation().length(),
            PROTECTED_FREE_SPACE_FLOOR
        );
        drop(context);
        drop(store);

        let store = OperationJournalStore::open(&fixture.owner).expect("open final-check store");
        let wrong_root = BoundDatabaseRoot::for_profile_child(
            &fixture.owner,
            Path::new("restart-recovery-wrong-root"),
        )
        .expect("wrong restart root");
        let owner = super::super::capacity_volume_owner::acquire_fixture_for_test(
            &descriptor,
            &registry_path,
            volume.clone(),
            fixture.owner.profile_root_identity(),
        )
        .expect("acquire owner for wrong-root check");
        let before_wrong_root = fs::metadata(&reserve_file_path)
            .expect("read reserve before wrong-root check")
            .len();
        let wrong_root_error = store
            .reopen_recovery_reserve(owner, &wrong_root, &fixture.owner)
            .err()
            .expect("wrong root binding must reject");
        assert!(matches!(
            wrong_root_error,
            RecoveryReserveContextError::Binding(_)
        ));
        assert_eq!(
            fs::metadata(&reserve_file_path)
                .expect("read reserve after wrong-root check")
                .len(),
            before_wrong_root
        );

        let owner = super::super::capacity_volume_owner::acquire_fixture_for_test(
            &descriptor,
            &registry_path,
            volume.clone(),
            fixture.owner.profile_root_identity(),
        )
        .expect("acquire owner for missing-binding check");
        store
            .connection
            .execute(
                &format!(
                    "DELETE FROM {} WHERE volume_device = ?1",
                    recovery_reserve_binding::TABLE_NAME
                ),
                params![volume.device.to_string()],
            )
            .expect("remove binding corruption fixture");
        let missing_error = store
            .reopen_recovery_reserve(owner, &reserve_root, &fixture.owner)
            .err()
            .expect("missing binding must require reconciliation");
        assert!(matches!(
            missing_error,
            RecoveryReserveContextError::Binding(
                recovery_reserve_binding::ReserveBindingError::Missing(_)
            )
        ));
        assert_eq!(
            fs::metadata(&reserve_file_path)
                .expect("read reserve after missing-binding check")
                .len(),
            before_wrong_root
        );
        drop(store);
    }

    #[test]
    fn typed_copy_insert_outcome_acknowledges_exact_readback() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let (operation_id, intent, universal_intent, command_args, capacity_plan) =
            v5_copy_inputs();

        let outcome = store.persist_copy_intent_with_outcome(
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
            50,
        );
        let receipt = outcome.receipt().expect("fully acknowledged copy insert");
        assert_eq!(receipt.operation_id(), operation_id);
        assert_eq!(receipt.sequence(), 1);
        assert_eq!(
            store
                .get(operation_id)
                .expect("read acknowledged copy row")
                .map(|record| record.operation_id),
            Some(operation_id)
        );
    }

    #[test]
    fn v9_render_insert_retry_cancel_and_restart_preserve_typed_observation() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let (operation_id, intent, universal_intent, command_args, capacity_plan) =
            v9_render_inputs("DuplicateDouble");
        let inserted = store
            .persist_command_intent_v9_with_outcome(
                operation_id,
                intent,
                universal_intent,
                UniversalCommandArgs::Render(command_args.clone()),
                capacity_plan,
                50,
            )
            .receipt()
            .expect("persist V9 render command");
        assert_eq!(inserted.sequence(), 1);

        let retry = RetryObservation::new(
            1,
            51,
            52,
            RetryLeaseId::new("render-retry-1").expect("retry lease"),
            53,
            RetryErrorClass::TransientContention,
            "render_busy",
            None,
        )
        .expect("retry observation");
        let retry_receipt = store
            .record_retry_pending(operation_id, inserted.sequence(), retry.clone())
            .expect("persist V9 retry");
        assert_eq!(retry_receipt.sequence(), 2);
        assert_eq!(
            store
                .record_retry_pending(operation_id, inserted.sequence(), retry.clone())
                .expect("replay V9 retry"),
            retry_receipt
        );

        let cancellation = CancellationRequestObservation::new(54).expect("cancel observation");
        let cancel_receipt = store
            .request_cancel_before_publish(
                operation_id,
                retry_receipt.sequence(),
                cancellation.clone(),
            )
            .expect("persist V9 cancellation");
        assert_eq!(cancel_receipt.sequence(), 3);
        let persisted = store
            .get(operation_id)
            .expect("get V9 render command")
            .expect("V9 render row");
        assert_eq!(persisted.schema_version, SCHEMA_V9);
        assert_eq!(
            persisted.universal_command_args(),
            Some(&UniversalCommandArgs::Render(command_args.clone()))
        );
        assert_eq!(persisted.retry_observation(), Some(&retry));
        assert_eq!(persisted.cancellation_request(), Some(&cancellation));
        drop(store);
        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen store");
        let restarted = reopened
            .get(operation_id)
            .expect("get restarted V9 render command")
            .expect("restarted V9 render row");
        assert_eq!(restarted.schema_version, SCHEMA_V9);
        assert_eq!(
            restarted.universal_command_args(),
            Some(&UniversalCommandArgs::Render(command_args))
        );
        assert_eq!(restarted.retry_observation(), Some(&retry));
        assert_eq!(restarted.cancellation_request(), Some(&cancellation));
    }

    #[test]
    fn v10_removal_insert_retry_cancel_and_restart_preserve_typed_observation() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let (operation_id, intent, universal_intent, command_args, capacity_plan) =
            v10_trash_inputs();
        let inserted = store
            .persist_command_intent_v10_with_outcome(
                operation_id,
                intent,
                universal_intent,
                UniversalCommandArgs::Removal(RemovalCommandArgs::Trash(command_args.clone())),
                capacity_plan,
                50,
            )
            .receipt()
            .expect("persist V10 removal command");
        assert_eq!(inserted.sequence(), 1);

        let retry = RetryObservation::new(
            1,
            51,
            52,
            RetryLeaseId::new("trash-retry-1").expect("retry lease"),
            53,
            RetryErrorClass::TransientContention,
            "trash_busy",
            None,
        )
        .expect("retry observation");
        let retry_receipt = store
            .record_retry_pending(operation_id, inserted.sequence(), retry.clone())
            .expect("persist V10 retry");
        assert_eq!(retry_receipt.sequence(), 2);
        assert_eq!(
            store
                .record_retry_pending(operation_id, inserted.sequence(), retry.clone())
                .expect("replay V10 retry"),
            retry_receipt
        );

        let stale = RetryObservation::new(
            2,
            54,
            55,
            RetryLeaseId::new("trash-retry-2").expect("next retry lease"),
            56,
            RetryErrorClass::TransientAvailability,
            "trash_unavailable",
            None,
        )
        .expect("next retry observation");
        assert!(matches!(
            store.record_retry_pending(operation_id, inserted.sequence(), stale),
            Err(OperationJournalStoreError::StaleSequence { .. })
        ));

        let cancellation = CancellationRequestObservation::new(57).expect("cancel observation");
        let cancel_receipt = store
            .request_cancel_before_publish(
                operation_id,
                retry_receipt.sequence(),
                cancellation.clone(),
            )
            .expect("persist V10 cancellation");
        assert_eq!(cancel_receipt.sequence(), 3);
        let persisted = store
            .get(operation_id)
            .expect("get V10 removal command")
            .expect("V10 removal row");
        assert_eq!(persisted.schema_version, SCHEMA_V10);
        assert_eq!(
            persisted.universal_command_args(),
            Some(&UniversalCommandArgs::Removal(RemovalCommandArgs::Trash(
                command_args.clone(),
            )))
        );
        assert_eq!(persisted.retry_observation(), Some(&retry));
        assert_eq!(persisted.cancellation_request(), Some(&cancellation));
        drop(store);
        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen store");
        let restarted = reopened
            .get(operation_id)
            .expect("get restarted V10 removal command")
            .expect("restarted V10 removal row");
        assert_eq!(restarted.schema_version, SCHEMA_V10);
        assert_eq!(
            restarted.universal_command_args(),
            Some(&UniversalCommandArgs::Removal(RemovalCommandArgs::Trash(
                command_args
            )))
        );
        assert_eq!(restarted.retry_observation(), Some(&retry));
        assert_eq!(restarted.cancellation_request(), Some(&cancellation));
    }

    #[test]
    fn v11_retained_resolution_insert_retry_cancel_and_restart_preserve_observation() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let (operation_id, intent, universal_intent, command_args, capacity_plan) =
            v11_retained_restore_inputs();
        let inserted = store
            .persist_command_intent_v11_with_outcome(
                operation_id,
                intent,
                universal_intent,
                UniversalCommandArgs::RetainedResolution(command_args.clone()),
                capacity_plan,
                50,
            )
            .receipt()
            .expect("persist V11 retained resolution command");
        assert_eq!(inserted.sequence(), 1);

        let retry = RetryObservation::new(
            1,
            51,
            52,
            RetryLeaseId::new("retained-retry-1").expect("retry lease"),
            53,
            RetryErrorClass::TransientAvailability,
            "retained_busy",
            None,
        )
        .expect("retry observation");
        let retry_receipt = store
            .record_retry_pending(operation_id, inserted.sequence(), retry.clone())
            .expect("persist V11 retry");
        assert_eq!(retry_receipt.sequence(), 2);
        assert_eq!(
            store
                .record_retry_pending(operation_id, inserted.sequence(), retry.clone())
                .expect("replay V11 retry"),
            retry_receipt
        );

        let stale = RetryObservation::new(
            2,
            54,
            55,
            RetryLeaseId::new("retained-retry-2").expect("next retry lease"),
            56,
            RetryErrorClass::TransientContention,
            "retained_again",
            None,
        )
        .expect("next retry observation");
        assert!(matches!(
            store.record_retry_pending(operation_id, inserted.sequence(), stale),
            Err(OperationJournalStoreError::StaleSequence { .. })
        ));

        let cancellation = CancellationRequestObservation::new(57).expect("cancel observation");
        let cancel_receipt = store
            .request_cancel_before_publish(
                operation_id,
                retry_receipt.sequence(),
                cancellation.clone(),
            )
            .expect("persist V11 cancellation");
        assert_eq!(cancel_receipt.sequence(), 3);
        assert_eq!(
            store
                .request_cancel_before_publish(
                    operation_id,
                    inserted.sequence(),
                    cancellation.clone(),
                )
                .expect("replay V11 cancellation"),
            cancel_receipt
        );
        let persisted = store
            .get(operation_id)
            .expect("get V11 retained command")
            .expect("V11 retained row");
        assert_eq!(persisted.schema_version, SCHEMA_V11);
        assert_eq!(
            persisted.universal_command_args(),
            Some(&UniversalCommandArgs::RetainedResolution(
                command_args.clone()
            ))
        );
        assert_eq!(persisted.retry_observation(), Some(&retry));
        assert_eq!(persisted.cancellation_request(), Some(&cancellation));
        drop(store);

        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen store");
        let restarted = reopened
            .get(operation_id)
            .expect("get restarted V11 retained command")
            .expect("restarted V11 retained row");
        assert_eq!(restarted.schema_version, SCHEMA_V11);
        assert_eq!(
            restarted.universal_command_args(),
            Some(&UniversalCommandArgs::RetainedResolution(command_args))
        );
        assert_eq!(restarted.retry_observation(), Some(&retry));
        assert_eq!(restarted.cancellation_request(), Some(&cancellation));
    }

    #[test]
    fn v12_selection_normalize_insert_retry_cancel_and_restart_preserve_observation() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let (operation_id, intent, universal_intent, command_args, capacity_plan) =
            v12_selection_normalize_inputs();
        let inserted = store
            .persist_selection_normalize_intent_with_outcome(
                operation_id,
                intent,
                universal_intent,
                command_args.clone(),
                capacity_plan,
                50,
            )
            .receipt()
            .expect("persist V12 selection Normalize command");
        assert_eq!(inserted.sequence(), 1);

        let retry = RetryObservation::new(
            1,
            51,
            52,
            RetryLeaseId::new("selection-retry-1").expect("retry lease"),
            53,
            RetryErrorClass::TransientAvailability,
            "selection_unavailable",
            None,
        )
        .expect("retry observation");
        let retry_receipt = store
            .record_retry_pending(operation_id, inserted.sequence(), retry.clone())
            .expect("persist V12 retry");
        assert_eq!(retry_receipt.sequence(), 2);
        assert_eq!(
            store
                .record_retry_pending(operation_id, inserted.sequence(), retry.clone())
                .expect("replay V12 retry"),
            retry_receipt
        );

        let stale = RetryObservation::new(
            2,
            54,
            55,
            RetryLeaseId::new("selection-retry-2").expect("next retry lease"),
            56,
            RetryErrorClass::TransientContention,
            "selection_again",
            None,
        )
        .expect("next retry observation");
        assert!(matches!(
            store.record_retry_pending(operation_id, inserted.sequence(), stale),
            Err(OperationJournalStoreError::StaleSequence { .. })
        ));

        let cancellation = CancellationRequestObservation::new(57).expect("cancel observation");
        let cancel_receipt = store
            .request_cancel_before_publish(
                operation_id,
                retry_receipt.sequence(),
                cancellation.clone(),
            )
            .expect("persist V12 cancellation");
        assert_eq!(cancel_receipt.sequence(), 3);
        assert_eq!(
            store
                .request_cancel_before_publish(
                    operation_id,
                    inserted.sequence(),
                    cancellation.clone(),
                )
                .expect("replay V12 cancellation"),
            cancel_receipt
        );
        let persisted = store
            .get(operation_id)
            .expect("get V12 selection Normalize command")
            .expect("V12 selection Normalize row");
        assert_eq!(persisted.schema_version, SCHEMA_V12);
        assert_eq!(
            persisted.universal_command_args(),
            Some(&UniversalCommandArgs::SelectionNormalize(
                command_args.clone()
            ))
        );
        assert_eq!(persisted.retry_observation(), Some(&retry));
        assert_eq!(persisted.cancellation_request(), Some(&cancellation));
        drop(store);

        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen store");
        let restarted = reopened
            .get(operation_id)
            .expect("get restarted V12 selection Normalize command")
            .expect("restarted V12 selection Normalize row");
        assert_eq!(restarted.schema_version, SCHEMA_V12);
        assert_eq!(
            restarted.universal_command_args(),
            Some(&UniversalCommandArgs::SelectionNormalize(command_args))
        );
        assert_eq!(restarted.retry_observation(), Some(&retry));
        assert_eq!(restarted.cancellation_request(), Some(&cancellation));
    }

    #[test]
    fn v13_extract_and_trim_insert_retry_cancel_and_restart_preserve_one_compound_operation() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let (operation_id, intent, universal_intent, command_args, capacity_plan) =
            v13_selection_output_inputs();
        let inserted = store
            .persist_command_intent_v13_with_outcome(
                operation_id,
                intent,
                universal_intent,
                UniversalCommandArgs::SelectionOutput(command_args.clone()),
                capacity_plan,
                50,
            )
            .receipt()
            .expect("persist V13 ExtractAndTrim command");
        assert_eq!(inserted.sequence(), 1);

        let retry = RetryObservation::new(
            1,
            51,
            52,
            RetryLeaseId::new("extract-retry-1").expect("retry lease"),
            53,
            RetryErrorClass::TransientAvailability,
            "extract_unavailable",
            None,
        )
        .expect("retry observation");
        let retry_receipt = store
            .record_retry_pending(operation_id, inserted.sequence(), retry.clone())
            .expect("persist V13 retry");
        assert_eq!(retry_receipt.sequence(), 2);
        assert_eq!(
            store
                .record_retry_pending(operation_id, inserted.sequence(), retry.clone())
                .expect("replay V13 retry"),
            retry_receipt
        );

        let cancellation = CancellationRequestObservation::new(57).expect("cancel observation");
        let cancel_receipt = store
            .request_cancel_before_publish(
                operation_id,
                retry_receipt.sequence(),
                cancellation.clone(),
            )
            .expect("persist V13 cancellation");
        assert_eq!(cancel_receipt.sequence(), 3);
        assert_eq!(
            store
                .request_cancel_before_publish(
                    operation_id,
                    inserted.sequence(),
                    cancellation.clone(),
                )
                .expect("replay V13 cancellation"),
            cancel_receipt
        );
        let persisted = store
            .get(operation_id)
            .expect("get V13 ExtractAndTrim command")
            .expect("V13 ExtractAndTrim row");
        assert_eq!(persisted.schema_version, SCHEMA_V13);
        assert_eq!(
            persisted.universal_command_args(),
            Some(&UniversalCommandArgs::SelectionOutput(command_args.clone()))
        );
        assert_eq!(persisted.retry_observation(), Some(&retry));
        assert_eq!(persisted.cancellation_request(), Some(&cancellation));
        drop(store);

        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen store");
        let restarted = reopened
            .get(operation_id)
            .expect("get restarted V13 ExtractAndTrim command")
            .expect("restarted V13 ExtractAndTrim row");
        assert_eq!(restarted.schema_version, SCHEMA_V13);
        assert_eq!(
            restarted.universal_command_args(),
            Some(&UniversalCommandArgs::SelectionOutput(command_args))
        );
        assert_eq!(restarted.retry_observation(), Some(&retry));
        assert_eq!(restarted.cancellation_request(), Some(&cancellation));
    }

    #[test]
    fn v14_history_insert_retry_cancel_and_restart_preserve_ordered_args() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let (operation_id, intent, universal_intent, command_args, capacity_plan) =
            v14_history_inputs(HistoryDirection::Undo);
        let inserted = store
            .persist_history_intent_with_outcome(
                operation_id,
                intent,
                universal_intent,
                command_args.clone(),
                capacity_plan,
                50,
            )
            .receipt()
            .expect("persist V14 history command");
        assert_eq!(inserted.sequence(), 1);

        let retry = RetryObservation::new(
            1,
            51,
            52,
            RetryLeaseId::new("history-retry-1").expect("retry lease"),
            53,
            RetryErrorClass::TransientAvailability,
            "history_unavailable",
            None,
        )
        .expect("retry observation");
        let retry_receipt = store
            .record_retry_pending(operation_id, inserted.sequence(), retry.clone())
            .expect("persist V14 retry");
        assert_eq!(retry_receipt.sequence(), 2);
        assert_eq!(
            store
                .record_retry_pending(operation_id, inserted.sequence(), retry.clone())
                .expect("replay V14 retry"),
            retry_receipt
        );

        let cancellation = CancellationRequestObservation::new(57).expect("cancel observation");
        let cancel_receipt = store
            .request_cancel_before_publish(
                operation_id,
                retry_receipt.sequence(),
                cancellation.clone(),
            )
            .expect("persist V14 cancellation");
        assert_eq!(cancel_receipt.sequence(), 3);
        assert_eq!(
            store
                .request_cancel_before_publish(
                    operation_id,
                    inserted.sequence(),
                    cancellation.clone(),
                )
                .expect("replay V14 cancellation"),
            cancel_receipt
        );
        let persisted = store
            .get(operation_id)
            .expect("get V14 history command")
            .expect("V14 history row");
        assert_eq!(persisted.schema_version, SCHEMA_V14);
        assert_eq!(
            persisted.universal_command_args(),
            Some(&UniversalCommandArgs::History(command_args.clone()))
        );
        assert_eq!(
            persisted
                .universal_intent
                .as_ref()
                .expect("history universal intent")
                .command_kind(),
            UniversalCommandKind::Undo
        );
        assert_eq!(persisted.retry_observation(), Some(&retry));
        assert_eq!(persisted.cancellation_request(), Some(&cancellation));
        drop(store);

        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen store");
        let restarted = reopened
            .get(operation_id)
            .expect("get restarted V14 history command")
            .expect("restarted V14 history row");
        assert_eq!(restarted.schema_version, SCHEMA_V14);
        assert_eq!(
            restarted.universal_command_args(),
            Some(&UniversalCommandArgs::History(command_args))
        );
        assert_eq!(restarted.retry_observation(), Some(&retry));
        assert_eq!(restarted.cancellation_request(), Some(&cancellation));
    }

    #[test]
    fn v15_external_reconciliation_insert_retry_cancel_and_restart_preserve_raw_batch() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let (operation_id, intent, universal_intent, command_args, capacity_plan) =
            v15_external_inputs();
        let inserted = store
            .persist_external_reconciliation_intent_with_outcome(
                operation_id,
                intent,
                universal_intent,
                command_args.clone(),
                capacity_plan,
                50,
            )
            .receipt()
            .expect("persist V15 external reconciliation command");
        assert_eq!(inserted.sequence(), 1);

        let retry = RetryObservation::new(
            1,
            51,
            52,
            RetryLeaseId::new("external-retry-1").expect("retry lease"),
            53,
            RetryErrorClass::TransientAvailability,
            "external_unavailable",
            None,
        )
        .expect("retry observation");
        let retry_receipt = store
            .record_retry_pending(operation_id, inserted.sequence(), retry.clone())
            .expect("persist V15 retry");
        assert_eq!(retry_receipt.sequence(), 2);
        assert_eq!(
            store
                .record_retry_pending(operation_id, inserted.sequence(), retry.clone())
                .expect("replay V15 retry"),
            retry_receipt
        );

        let cancellation = CancellationRequestObservation::new(57).expect("cancel observation");
        let cancel_receipt = store
            .request_cancel_before_publish(
                operation_id,
                retry_receipt.sequence(),
                cancellation.clone(),
            )
            .expect("persist V15 cancellation");
        assert_eq!(cancel_receipt.sequence(), 3);
        assert_eq!(
            store
                .request_cancel_before_publish(
                    operation_id,
                    inserted.sequence(),
                    cancellation.clone(),
                )
                .expect("replay V15 cancellation"),
            cancel_receipt
        );
        let persisted = store
            .get(operation_id)
            .expect("get V15 external command")
            .expect("V15 external row");
        assert_eq!(persisted.schema_version, SCHEMA_V15);
        assert_eq!(
            persisted.universal_command_args(),
            Some(&UniversalCommandArgs::ExternalReconciliation(
                command_args.clone()
            ))
        );
        assert_eq!(persisted.retry_observation(), Some(&retry));
        assert_eq!(persisted.cancellation_request(), Some(&cancellation));
        drop(store);

        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen store");
        let restarted = reopened
            .get(operation_id)
            .expect("get restarted V15 external command")
            .expect("restarted V15 external row");
        assert_eq!(restarted.schema_version, SCHEMA_V15);
        assert_eq!(
            restarted.universal_command_args(),
            Some(&UniversalCommandArgs::ExternalReconciliation(command_args))
        );
        assert_eq!(restarted.retry_observation(), Some(&retry));
        assert_eq!(restarted.cancellation_request(), Some(&cancellation));
    }

    #[test]
    fn typed_insert_outcome_reports_prebegin_and_validation_failures_without_receipts() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");

        let invalid_id = OperationId::new();
        let mut invalid_record = intent_record(invalid_id);
        invalid_record.phase = OperationPhase::Prepared;
        let invalid = store.insert_intent_with_outcome(&invalid_record);
        let failure = invalid.failure().expect("invalid argument failure");
        assert_eq!(failure.operation_id(), invalid_id);
        assert_eq!(
            failure.stage(),
            JournalInsertWriteStage::NoMutationAttempted
        );
        assert_eq!(failure.attempted_sequence(), None);
        assert!(matches!(
            failure.error(),
            OperationJournalStoreError::NotIntentDurable {
                phase: OperationPhase::Prepared
            }
        ));

        let prebegin_id = OperationId::new();
        store.set_insert_test_hook(|boundary| {
            if boundary == InsertTestBoundary::BeforeBegin {
                InsertTestDirective::Fail(OperationJournalStoreError::IdentityCheckUnavailable {
                    reason: String::from("forced prebegin failure"),
                })
            } else {
                InsertTestDirective::Continue
            }
        });
        let prebegin = store.insert_intent_with_outcome(&intent_record(prebegin_id));
        let failure = prebegin.failure().expect("prebegin failure");
        assert_eq!(failure.operation_id(), prebegin_id);
        assert_eq!(
            failure.stage(),
            JournalInsertWriteStage::NoMutationAttempted
        );
        assert_eq!(failure.attempted_sequence(), None);
        assert!(failure.rollback_error().is_none());
        assert!(matches!(
            failure.error(),
            OperationJournalStoreError::IdentityCheckUnavailable { reason }
                if reason == "forced prebegin failure"
        ));
        assert!(prebegin.receipt().is_none());

        let validation_id = OperationId::new();
        store.set_insert_test_hook(|boundary| {
            if boundary == InsertTestBoundary::BeforePrecommitValidation {
                InsertTestDirective::Fail(OperationJournalStoreError::IdentityCheckUnavailable {
                    reason: String::from("forced precommit failure"),
                })
            } else {
                InsertTestDirective::Continue
            }
        });
        let validation = store.insert_intent_with_outcome(&intent_record(validation_id));
        let failure = validation.failure().expect("precommit failure");
        assert_eq!(
            failure.stage(),
            JournalInsertWriteStage::RolledBackBeforeCommit
        );
        assert_eq!(failure.attempted_sequence(), Some(1));
        assert!(failure.rollback_error().is_none());
        assert!(matches!(
            failure.error(),
            OperationJournalStoreError::IdentityCheckUnavailable { reason }
                if reason == "forced precommit failure"
        ));
        assert_eq!(
            store.get(validation_id).expect("read rolled back row"),
            None
        );
        let max_sequence: Option<i64> = store
            .connection
            .query_row("SELECT MAX(sequence) FROM operation_journal", [], |row| {
                row.get(0)
            })
            .expect("read sequence after rollback");
        assert_eq!(max_sequence, None);
    }

    #[test]
    fn typed_duplicate_outcome_keeps_existing_row_without_absence_claim() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let record = intent_record(OperationId::new());
        let original = store.insert_intent(&record).expect("insert original");

        let outcome = store.insert_intent_with_outcome(&record);
        let failure = outcome.failure().expect("duplicate failure");
        assert_eq!(failure.operation_id(), record.operation_id);
        assert_eq!(
            failure.stage(),
            JournalInsertWriteStage::RolledBackBeforeCommit
        );
        assert_eq!(failure.attempted_sequence(), None);
        assert!(matches!(
            failure.error(),
            OperationJournalStoreError::DuplicateOperation { operation_id }
                if *operation_id == record.operation_id
        ));
        assert_eq!(
            store.get(record.operation_id).expect("read original"),
            Some(record)
        );
        assert_eq!(
            journal_sequence(&store, original.operation_id()),
            original.sequence()
        );
    }

    #[cfg(unix)]
    #[test]
    fn typed_insert_rejects_namespace_replacement_after_insert_and_restores_original() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let database = database_path(&fixture.owner);
        let displaced = database.with_file_name("journal.sqlite3.insert-displaced");
        let replacement = b"replacement database bytes".to_vec();
        let replacement_for_hook = replacement.clone();
        let database_for_hook = database.clone();
        let displaced_for_hook = displaced.clone();
        store.set_insert_test_hook(move |boundary| {
            if boundary == InsertTestBoundary::BeforePrecommitValidation {
                fs::rename(&database_for_hook, &displaced_for_hook)
                    .expect("displace database after insert");
                fs::write(&database_for_hook, &replacement_for_hook)
                    .expect("install replacement database");
            }
            InsertTestDirective::Continue
        });

        let operation_id = OperationId::new();
        let outcome = store.insert_intent_with_outcome(&intent_record(operation_id));
        let failure = outcome.failure().expect("namespace replacement failure");
        assert_eq!(
            failure.stage(),
            JournalInsertWriteStage::RolledBackBeforeCommit
        );
        assert!(matches!(
            failure.error(),
            OperationJournalStoreError::DatabaseReplaced { .. }
        ));
        assert!(failure.rollback_error().is_none());
        assert_eq!(
            fs::read(&database).expect("replacement remains untouched"),
            replacement
        );

        drop(store);
        fs::remove_file(&database).expect("remove replacement database");
        fs::rename(&displaced, &database).expect("restore original database");
        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen original");
        assert_eq!(
            reopened.get(operation_id).expect("read restored journal"),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn typed_insert_reports_committed_but_unacknowledged_after_namespace_replacement() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let database = database_path(&fixture.owner);
        let displaced = database.with_file_name("journal.sqlite3.postcommit-displaced");
        let replacement = b"replacement after commit".to_vec();
        let replacement_for_hook = replacement.clone();
        let database_for_hook = database.clone();
        let displaced_for_hook = displaced.clone();
        store.set_insert_test_hook(move |boundary| {
            if boundary == InsertTestBoundary::AfterCommit {
                fs::rename(&database_for_hook, &displaced_for_hook)
                    .expect("displace database after commit");
                fs::write(&database_for_hook, &replacement_for_hook)
                    .expect("install replacement after commit");
            }
            InsertTestDirective::Continue
        });

        let operation_id = OperationId::new();
        let record = intent_record(operation_id);
        let outcome = store.insert_intent_with_outcome(&record);
        let failure = outcome
            .failure()
            .expect("postcommit acknowledgement failure");
        assert_eq!(
            failure.stage(),
            JournalInsertWriteStage::CommittedButNotAcknowledged
        );
        assert!(matches!(
            failure.error(),
            OperationJournalStoreError::DatabaseReplaced { .. }
        ));
        assert_eq!(
            fs::read(&database).expect("replacement remains untouched"),
            replacement
        );

        drop(store);
        fs::remove_file(&database).expect("remove replacement database");
        fs::rename(&displaced, &database).expect("restore committed database");
        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen committed");
        assert_eq!(
            reopened.get(operation_id).expect("read committed row"),
            Some(record)
        );
    }

    #[test]
    fn typed_insert_keeps_unconfirmed_rollback_closed_to_public_reads_and_writes() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let operation_id = OperationId::new();
        store.set_insert_test_hook(|boundary| match boundary {
            InsertTestBoundary::BeforePrecommitValidation | InsertTestBoundary::Rollback => {
                InsertTestDirective::FailAndLeaveForRollback(
                    OperationJournalStoreError::IdentityCheckUnavailable {
                        reason: String::from("forced rollback uncertainty"),
                    },
                )
            }
            _ => InsertTestDirective::Continue,
        });

        let outcome = store.insert_intent_with_outcome(&intent_record(operation_id));
        let failure = outcome.failure().expect("unconfirmed rollback failure");
        assert_eq!(
            failure.stage(),
            JournalInsertWriteStage::RollbackNotConfirmed
        );
        assert_eq!(failure.attempted_sequence(), Some(1));
        assert!(matches!(
            failure.rollback_error(),
            Some(OperationJournalStoreError::IdentityCheckUnavailable { reason })
                if reason == "forced rollback uncertainty"
        ));

        assert!(matches!(
            store.get(operation_id),
            Err(OperationJournalStoreError::RequiresAudit { reason, .. })
                if reason.contains("active transaction")
        ));
        assert!(matches!(
            store.scan_recovery_page(None),
            Err(RecoveryScanError::Store(OperationJournalStoreError::RequiresAudit {
                reason, ..
            })) if reason.contains("active transaction")
        ));
        let another_id = OperationId::new();
        let another = store.insert_intent_with_outcome(&intent_record(another_id));
        assert!(matches!(
            another.failure().map(JournalInsertFailure::error),
            Some(OperationJournalStoreError::RequiresAudit { reason, .. })
                if reason.contains("active transaction")
        ));

        // The test hook deliberately leaves the transaction open. Restore the hook and clean it
        // up through the private connection before making any further public observations.
        store.set_insert_test_hook(|_| InsertTestDirective::Continue);
        store
            .connection
            .execute_batch("ROLLBACK")
            .expect("test cleanup rollback");
        assert_eq!(store.get(operation_id).expect("read after cleanup"), None);
    }

    #[test]
    fn typed_insert_commit_and_postcommit_outcomes_are_conservative() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");

        let before_commit_id = OperationId::new();
        store.set_insert_test_hook(|boundary| {
            if boundary == InsertTestBoundary::Commit {
                InsertTestDirective::Fail(OperationJournalStoreError::IdentityCheckUnavailable {
                    reason: String::from("forced commit failure"),
                })
            } else {
                InsertTestDirective::Continue
            }
        });
        let before_commit = store.insert_intent_with_outcome(&intent_record(before_commit_id));
        let failure = before_commit.failure().expect("commit failure");
        assert_eq!(
            failure.stage(),
            JournalInsertWriteStage::CommitOutcomeUnknown
        );
        assert!(failure.rollback_error().is_none());
        assert_eq!(
            store
                .get(before_commit_id)
                .expect("read rolled back commit"),
            None
        );

        let execute_then_fail_id = OperationId::new();
        store.set_insert_test_hook(|boundary| {
            if boundary == InsertTestBoundary::Commit {
                InsertTestDirective::ExecuteThenFail(
                    OperationJournalStoreError::IdentityCheckUnavailable {
                        reason: String::from("commit result unavailable"),
                    },
                )
            } else {
                InsertTestDirective::Continue
            }
        });
        let execute_then_fail =
            store.insert_intent_with_outcome(&intent_record(execute_then_fail_id));
        let failure = execute_then_fail
            .failure()
            .expect("execute-then-fail commit outcome");
        assert_eq!(
            failure.stage(),
            JournalInsertWriteStage::CommitOutcomeUnknown
        );
        assert!(failure.rollback_error().is_some());
        assert_eq!(
            store
                .get(execute_then_fail_id)
                .expect("read committed but unknown row")
                .map(|record| record.operation_id),
            Some(execute_then_fail_id)
        );

        let postcommit_id = OperationId::new();
        store.set_insert_test_hook(|boundary| {
            if boundary == InsertTestBoundary::AfterCommit {
                InsertTestDirective::Fail(OperationJournalStoreError::IdentityCheckUnavailable {
                    reason: String::from("forced acknowledgement failure"),
                })
            } else {
                InsertTestDirective::Continue
            }
        });
        let postcommit = store.insert_intent_with_outcome(&intent_record(postcommit_id));
        let failure = postcommit
            .failure()
            .expect("postcommit acknowledgement failure");
        assert_eq!(
            failure.stage(),
            JournalInsertWriteStage::CommittedButNotAcknowledged
        );
        assert_eq!(
            store
                .get(postcommit_id)
                .expect("read committed unacknowledged row")
                .map(|record| record.operation_id),
            Some(postcommit_id)
        );
    }

    #[test]
    fn open_retain_owner_and_child_lease_until_store_drop() {
        let mut fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let path = database_path(&fixture.owner);
        assert!(path.is_file());
        drop(fixture.owner);
        assert!(matches!(
            WritableProfileGuard::acquire_current(),
            Err(ProfileOwnershipError::ProfileOwnedByAnotherProcess { .. })
        ));
        drop(store);
        fixture.owner =
            WritableProfileGuard::acquire_current().expect("reacquire after store drop");
    }

    #[test]
    fn intent_insert_get_duplicate_and_restart_preserve_sequence() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let first = intent_record(OperationId::new());
        let receipt = store.insert_intent(&first).expect("insert first intent");
        assert_eq!(receipt.operation_id(), first.operation_id);
        assert_eq!(receipt.sequence(), 1);
        assert_eq!(
            store.get(first.operation_id).expect("get first").as_ref(),
            Some(&first)
        );
        assert!(matches!(
            store.insert_intent(&first),
            Err(OperationJournalStoreError::DuplicateOperation { operation_id })
                if operation_id == first.operation_id
        ));

        let second = intent_record(OperationId::new());
        let second_receipt = store.insert_intent(&second).expect("insert second intent");
        assert_eq!(second_receipt.sequence(), 2);
        drop(store);

        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen store");
        assert_eq!(
            reopened
                .get(first.operation_id)
                .expect("get persisted first"),
            Some(first)
        );
        assert_eq!(
            reopened
                .get(second.operation_id)
                .expect("get persisted second"),
            Some(second)
        );
    }

    #[test]
    fn v3_intent_insert_get_and_restart_preserve_universal_participants() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let record = universal_intent_record(OperationId::new());
        let receipt = store.insert_intent(&record).expect("insert v3 intent");
        assert_eq!(receipt.operation_id(), record.operation_id);
        assert_eq!(
            record
                .universal_intent
                .as_ref()
                .expect("universal intent")
                .participants()
                .descriptors()
                .len(),
            1
        );
        assert_eq!(
            store.get(record.operation_id).expect("get v3 intent"),
            Some(record.clone())
        );
        drop(store);

        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen store");
        assert_eq!(
            reopened
                .get(record.operation_id)
                .expect("get restarted v3 intent"),
            Some(record)
        );
    }

    #[test]
    fn v5_copy_retry_and_cancel_preserve_typed_envelope_and_cas_semantics() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let (operation_id, intent, universal_intent, command_args, capacity_plan) =
            v5_copy_inputs();
        let inserted = store
            .persist_copy_intent(
                operation_id,
                intent.clone(),
                universal_intent.clone(),
                command_args.clone(),
                capacity_plan.clone(),
                50,
            )
            .expect("persist v5 copy intent");
        assert_eq!(inserted.sequence(), 1);

        let first_retry = RetryObservation::new(
            1,
            51,
            52,
            RetryLeaseId::new("retry-lease-1").expect("retry lease"),
            53,
            RetryErrorClass::TransientContention,
            "writer_busy",
            Some(String::from("source database is busy")),
        )
        .expect("first retry observation");
        let retry_receipt = store
            .record_retry_pending(operation_id, inserted.sequence(), first_retry.clone())
            .expect("persist first retry");
        assert_eq!(retry_receipt.sequence(), 2);

        let persisted = store
            .get(operation_id)
            .expect("get retry row")
            .expect("retry row");
        assert_eq!(persisted.schema_version, SCHEMA_V5);
        assert_eq!(persisted.intent, intent);
        assert_eq!(persisted.universal_intent, Some(universal_intent));
        assert_eq!(persisted.command_args(), Some(&command_args));
        assert_eq!(persisted.capacity_plan, Some(capacity_plan));
        assert_eq!(persisted.retry_observation(), Some(&first_retry));
        assert_eq!(persisted.disposition, OperationDisposition::RetryPending);

        let page_after_old_cursor = store
            .scan_recovery_page(Some(
                RecoveryCursor::new(inserted.sequence()).expect("positive recovery cursor"),
            ))
            .expect("reobserve retry row after old cursor");
        assert_eq!(page_after_old_cursor.entries().len(), 1);
        assert_eq!(page_after_old_cursor.entries()[0].sequence(), 2);
        assert!(matches!(
            page_after_old_cursor.entries()[0].outcome(),
            RecoveryRowOutcome::Consistent(observed)
                if observed.retry_observation() == Some(&first_retry)
        ));

        // Replaying from the original observation is read-only and does not allocate a new
        // global sequence.
        let replay = store
            .record_retry_pending(operation_id, inserted.sequence(), first_retry.clone())
            .expect("replay first retry");
        assert_eq!(replay, retry_receipt);
        assert_eq!(journal_sequence(&store, operation_id), 2);

        let stale_different = RetryObservation::new(
            2,
            53,
            54,
            RetryLeaseId::new("retry-lease-2").expect("retry lease"),
            55,
            RetryErrorClass::TransientAvailability,
            "source_unavailable",
            None,
        )
        .expect("second retry observation");
        assert!(matches!(
            store.record_retry_pending(operation_id, inserted.sequence(), stale_different.clone()),
            Err(OperationJournalStoreError::StaleSequence { operation_id: actual, expected: 1, actual: 2 })
                if actual == operation_id
        ));
        assert_eq!(journal_sequence(&store, operation_id), 2);

        // Another operation advances the global sequence; the next update still uses MAX+1.
        let (second_id, second_intent, second_universal, second_args, second_plan) =
            v5_copy_inputs();
        let second_inserted = store
            .persist_copy_intent(
                second_id,
                second_intent,
                second_universal,
                second_args,
                second_plan,
                60,
            )
            .expect("persist second copy intent");
        assert_eq!(second_inserted.sequence(), 3);

        let second_retry_receipt = store
            .record_retry_pending(
                operation_id,
                retry_receipt.sequence(),
                stale_different.clone(),
            )
            .expect("persist second retry");
        assert_eq!(second_retry_receipt.sequence(), 4);

        let cancellation = CancellationRequestObservation::new(56).expect("cancel timestamp");
        let cancel_receipt = store
            .request_cancel_before_publish(
                operation_id,
                second_retry_receipt.sequence(),
                cancellation,
            )
            .expect("persist cancellation over retry");
        assert_eq!(cancel_receipt.sequence(), 5);
        let canceled = store
            .get(operation_id)
            .expect("get canceled v5 row")
            .expect("canceled v5 row");
        assert_eq!(canceled.schema_version, SCHEMA_V5);
        assert_eq!(
            canceled.disposition,
            OperationDisposition::CancelRequestedBeforePublish
        );
        assert_eq!(canceled.command_args(), Some(&command_args));
        assert_eq!(canceled.retry_observation(), Some(&stale_different));
        assert_eq!(canceled.cancellation_request(), Some(&cancellation));

        let cancel_replay = store
            .request_cancel_before_publish(operation_id, inserted.sequence(), cancellation)
            .expect("replay cancellation from stale sequence");
        assert_eq!(cancel_replay, cancel_receipt);
        assert_eq!(journal_sequence(&store, operation_id), 5);

        let retry_after_cancel = RetryObservation::new(
            3,
            57,
            58,
            RetryLeaseId::new("retry-lease-3").expect("retry lease"),
            59,
            RetryErrorClass::TransientContention,
            "writer_busy",
            None,
        )
        .expect("retry after cancellation");
        assert!(matches!(
            store.record_retry_pending(operation_id, cancel_receipt.sequence(), retry_after_cancel),
            Err(OperationJournalStoreError::RetryObservationConflict { operation_id: actual })
                if actual == operation_id
        ));
        assert_eq!(journal_sequence(&store, operation_id), 5);
    }

    #[test]
    fn staging_checkpoint_requires_exact_durable_v16_participant_and_acknowledgement() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let (operation_id, intent, universal_intent, command_args, legacy_plan) = v5_copy_inputs();
        let participant_id = universal_intent
            .participants()
            .descriptors()
            .iter()
            .find(|participant| participant.kind() == ParticipantKind::Filesystem)
            .expect("filesystem participant")
            .id();
        let full_plan = FullCapacityPlan::from_legacy(&legacy_plan).expect("full plan");
        let operation = StoredOperationRecord::new_v16_command(
            operation_id,
            intent,
            universal_intent,
            UniversalCommandArgs::Copy(command_args),
            full_plan,
            50,
        );
        let inserted = store.insert_intent(&operation).expect("persist V16 intent");
        let output_id = Uuid::new_v4();
        let make_record = |hash: u8| {
            staging_checkpoints::StagingCheckpointRecord::decode(
                &serde_json::to_vec(&serde_json::json!({
                    "schema_version": 1,
                    "operation_id": operation_id,
                    "participant_id": participant_id,
                    "effect_id": Uuid::new_v4(),
                    "output_id": output_id,
                    "staging_root": {
                        "owner": Uuid::new_v4(),
                        "registration_id": Uuid::new_v4(),
                        "generation": 1
                    },
                    "source_root": null,
                    "source_fence": null,
                    "locator": "staged.wav",
                    "logical_object_id": Uuid::new_v4(),
                    "physical_object_id": "physical-staged-file",
                    "bytes": 4,
                    "content_hash": vec![hash; 32],
                    "request_fingerprint": vec![1; 32]
                }))
                .expect("encode checkpoint fixture"),
            )
            .expect("decode checkpoint fixture")
        };
        let checkpoint = make_record(2);
        store.set_insert_test_hook(|boundary| {
            if boundary == InsertTestBoundary::BeforePrecommitValidation {
                InsertTestDirective::Fail(OperationJournalStoreError::IdentityCheckUnavailable {
                    reason: String::from("injected precommit failure"),
                })
            } else {
                InsertTestDirective::Continue
            }
        });
        assert!(matches!(
            store.checkpoint_staging(&checkpoint),
            Err(OperationJournalStoreError::StagingCheckpoint { .. })
        ));
        assert!(
            store
                .read_staging_checkpoint_row(output_id)
                .expect("read rolled-back checkpoint")
                .is_none()
        );

        let first = store
            .checkpoint_staging(&checkpoint)
            .expect("checkpoint exact staging");
        let checkpoint_sequence = match first {
            staging_checkpoints::StagingCheckpointReceipt::Committed { sequence } => sequence,
            staging_checkpoints::StagingCheckpointReceipt::Replay { .. } => {
                panic!("new checkpoint cannot replay")
            }
        };
        assert!(matches!(
            first,
            staging_checkpoints::StagingCheckpointReceipt::Committed { .. }
        ));
        assert!(matches!(
            store
                .checkpoint_staging(&checkpoint)
                .expect("read-only replay"),
            staging_checkpoints::StagingCheckpointReceipt::Replay { .. }
        ));
        assert!(store.checkpoint_staging(&make_record(3)).is_err());
        assert!(store.prove_clean_handoff_durable().is_err());
        assert_eq!(journal_sequence(&store, operation_id), inserted.sequence());
        drop(store);
        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen journal");
        assert!(matches!(
            reopened
                .checkpoint_staging(&checkpoint)
                .expect("replay after restart"),
            staging_checkpoints::StagingCheckpointReceipt::Replay { .. }
        ));
        reopened.set_insert_test_hook(|boundary| {
            if boundary == InsertTestBoundary::BeforePrecommitValidation {
                InsertTestDirective::Fail(OperationJournalStoreError::IdentityCheckUnavailable {
                    reason: String::from("injected grant precommit failure"),
                })
            } else {
                InsertTestDirective::Continue
            }
        });
        assert!(
            reopened
                .reserve_staging_grant_record(&checkpoint, checkpoint_sequence)
                .is_err()
        );

        assert!(
            !reopened
                .read_staging_checkpoint_row(output_id)
                .expect("read unreserved row")
                .expect("retained checkpoint")
                .2
        );
        reopened.set_insert_test_hook(|boundary| {
            if boundary == InsertTestBoundary::AfterCommit {
                InsertTestDirective::Fail(OperationJournalStoreError::IdentityCheckUnavailable {
                    reason: String::from("injected grant postcommit uncertainty"),
                })
            } else {
                InsertTestDirective::Continue
            }
        });
        assert!(matches!(
            reopened.reserve_staging_grant_record(&checkpoint, checkpoint_sequence),
            Err(OperationJournalStoreError::StagingOutcomeUncertain { .. })
        ));

        let uncertain_output = Uuid::new_v4();
        let mut uncertain_value = serde_json::to_value(&checkpoint).expect("checkpoint value");
        uncertain_value["output_id"] = serde_json::json!(uncertain_output);
        uncertain_value["effect_id"] = serde_json::json!(Uuid::new_v4());
        let uncertain = staging_checkpoints::StagingCheckpointRecord::decode(
            &serde_json::to_vec(&uncertain_value).expect("encode uncertain checkpoint"),
        )
        .expect("decode uncertain checkpoint");
        reopened.set_insert_test_hook(|boundary| {
            if boundary == InsertTestBoundary::AfterCommit {
                InsertTestDirective::Fail(OperationJournalStoreError::IdentityCheckUnavailable {
                    reason: String::from("injected checkpoint postcommit uncertainty"),
                })
            } else {
                InsertTestDirective::Continue
            }
        });
        assert!(matches!(
            reopened.checkpoint_staging(&uncertain),
            Err(OperationJournalStoreError::StagingOutcomeUncertain { .. })
        ));
        assert!(matches!(
            reopened
                .checkpoint_staging(&uncertain)
                .expect("retained replay"),
            staging_checkpoints::StagingCheckpointReceipt::Replay { .. }
        ));
        let cancellation = CancellationRequestObservation::new(51).expect("cancel observation");
        // Real SQLite COMMIT refusal leaves no row, but its unconfirmed durable outcome
        // is conservatively distinct from a known precommit rejection.
        let mut commit_value = uncertain_value.clone();
        let commit_output = Uuid::new_v4();
        commit_value["output_id"] = serde_json::json!(commit_output);
        commit_value["effect_id"] = serde_json::json!(Uuid::new_v4());
        let commit_record = staging_checkpoints::StagingCheckpointRecord::decode(
            &serde_json::to_vec(&commit_value).unwrap(),
        )
        .unwrap();
        reopened
            .connection
            .authorizer(Some(|context: rusqlite::hooks::AuthContext<'_>| {
                if matches!(
                    context.action,
                    rusqlite::hooks::AuthAction::Transaction {
                        // rusqlite 0.37 maps SQLite's COMMIT authorization event to Unknown.
                        operation: rusqlite::hooks::TransactionOperation::Unknown
                    }
                ) {
                    rusqlite::hooks::Authorization::Deny
                } else {
                    rusqlite::hooks::Authorization::Allow
                }
            }));
        assert!(matches!(
            reopened.checkpoint_staging(&commit_record),
            Err(OperationJournalStoreError::StagingOutcomeUncertain { .. })
        ));
        reopened.connection.authorizer(
            None::<fn(rusqlite::hooks::AuthContext<'_>) -> rusqlite::hooks::Authorization>,
        );
        assert!(
            reopened
                .read_staging_checkpoint_row(commit_output)
                .unwrap()
                .is_none()
        );

        let mut rollback_value = uncertain_value.clone();
        let rollback_output = Uuid::new_v4();
        rollback_value["output_id"] = serde_json::json!(rollback_output);
        rollback_value["effect_id"] = serde_json::json!(Uuid::new_v4());
        let rollback_record = staging_checkpoints::StagingCheckpointRecord::decode(
            &serde_json::to_vec(&rollback_value).unwrap(),
        )
        .unwrap();
        reopened.set_insert_test_hook(|boundary| {
            if boundary == InsertTestBoundary::BeforePrecommitValidation {
                InsertTestDirective::Fail(OperationJournalStoreError::IdentityCheckUnavailable {
                    reason: "force precommit rejection before denied rollback".into(),
                })
            } else {
                InsertTestDirective::Continue
            }
        });
        reopened
            .connection
            .authorizer(Some(|context: rusqlite::hooks::AuthContext<'_>| {
                if matches!(
                    context.action,
                    rusqlite::hooks::AuthAction::Transaction {
                        operation: rusqlite::hooks::TransactionOperation::Rollback
                    }
                ) {
                    rusqlite::hooks::Authorization::Deny
                } else {
                    rusqlite::hooks::Authorization::Allow
                }
            }));
        assert!(matches!(
            reopened.checkpoint_staging(&rollback_record),
            Err(OperationJournalStoreError::StagingOutcomeUncertain { .. })
        ));
        reopened.connection.authorizer(
            None::<fn(rusqlite::hooks::AuthContext<'_>) -> rusqlite::hooks::Authorization>,
        );
        assert!(!reopened.connection.is_autocommit());
        reopened.connection.execute_batch("ROLLBACK").unwrap();
        assert!(
            reopened
                .read_staging_checkpoint_row(rollback_output)
                .unwrap()
                .is_none()
        );

        // Deny the actual postcommit staging-row read, rather than simulating a marker.
        for grant_readback in [false, true] {
            let mut value = uncertain_value.clone();
            let output = Uuid::new_v4();
            value["output_id"] = serde_json::json!(output);
            value["effect_id"] = serde_json::json!(Uuid::new_v4());
            let record = staging_checkpoints::StagingCheckpointRecord::decode(
                &serde_json::to_vec(&value).unwrap(),
            )
            .unwrap();
            let sequence = if grant_readback {
                match reopened.checkpoint_staging(&record).unwrap() {
                    staging_checkpoints::StagingCheckpointReceipt::Committed { sequence } => {
                        sequence
                    }
                    _ => panic!("fresh checkpoint must commit"),
                }
            } else {
                0
            };
            let committed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let hook_flag = committed.clone();
            reopened.set_insert_test_hook(move |boundary| {
                if boundary == InsertTestBoundary::AfterCommit {
                    hook_flag.store(true, std::sync::atomic::Ordering::Release);
                }
                InsertTestDirective::Continue
            });
            let read_flag = committed.clone();
            reopened.connection.authorizer(Some(
                move |context: rusqlite::hooks::AuthContext<'_>| {
                    if read_flag.load(std::sync::atomic::Ordering::Acquire)
                        && matches!(
                            context.action,
                            rusqlite::hooks::AuthAction::Read {
                                table_name: "operation_journal_staging_checkpoints",
                                ..
                            }
                        )
                    {
                        rusqlite::hooks::Authorization::Deny
                    } else {
                        rusqlite::hooks::Authorization::Allow
                    }
                },
            ));
            let failed = if grant_readback {
                reopened
                    .reserve_staging_grant_record(&record, sequence)
                    .map(|_| ())
            } else {
                reopened.checkpoint_staging(&record).map(|_| ())
            };
            assert!(matches!(
                failed,
                Err(OperationJournalStoreError::StagingOutcomeUncertain { .. })
            ));
            reopened.connection.authorizer(
                None::<fn(rusqlite::hooks::AuthContext<'_>) -> rusqlite::hooks::Authorization>,
            );
            reopened.set_insert_test_hook(|_| InsertTestDirective::Continue);
            let row = reopened
                .read_staging_checkpoint_row(output)
                .unwrap()
                .unwrap();
            assert_eq!(row.1, record);
            assert_eq!(row.2, grant_readback);
            if grant_readback {
                assert!(
                    reopened
                        .reserve_staging_grant_record(&record, sequence)
                        .is_err()
                );
            } else {
                assert!(matches!(
                    reopened.checkpoint_staging(&record).unwrap(),
                    staging_checkpoints::StagingCheckpointReceipt::Replay { .. }
                ));
            }
        }
        reopened
            .request_cancel_before_publish(operation_id, inserted.sequence(), cancellation)
            .expect("durable cancellation");
        let uncertain_sequence = reopened
            .read_staging_checkpoint_row(uncertain_output)
            .expect("uncertain checkpoint read")
            .expect("uncertain row retained")
            .0;
        assert!(
            reopened
                .reserve_staging_grant_record(&uncertain, uncertain_sequence)
                .is_err()
        );
        assert!(
            reopened
                .read_staging_checkpoint_row(output_id)
                .expect("read reserved row")
                .expect("retained checkpoint")
                .2
        );
        assert!(
            reopened
                .reserve_staging_grant_record(&checkpoint, checkpoint_sequence)
                .is_err()
        );
    }

    #[test]
    fn chunk_staging_checkpoint_retains_charge_across_faults_replay_and_restart() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let (operation_id, intent, universal, args, legacy_plan) = v5_copy_inputs();
        let filesystem = universal
            .participants()
            .descriptors()
            .iter()
            .find(|participant| participant.kind() == ParticipantKind::Filesystem)
            .expect("filesystem participant")
            .id();
        let source_fence = universal
            .participants()
            .descriptors()
            .iter()
            .find_map(|participant| participant.source_fence())
            .expect("source participant fence")
            .clone();
        let operation = StoredOperationRecord::new_v16_command(
            operation_id,
            intent,
            universal,
            UniversalCommandArgs::Copy(args),
            FullCapacityPlan::from_legacy(&legacy_plan).expect("full plan"),
            50,
        );
        let operation_receipt = store.insert_intent(&operation).expect("persist operation");
        let row = &legacy_plan.volumes[0];
        let chunk_id = ChunkId::new();
        let request = ChunkClaimRequest::new(
            chunk_id,
            operation_id,
            AdmissionClaim::new(
                AdmissionClaimId::new(),
                row.identity.clone(),
                row.allocation_class.into(),
                row.allocated_bytes,
            )
            .expect("admission claim"),
            filesystem,
            String::from("partial.wav"),
            0,
            row.logical_bytes,
            row.allocation_unit,
            row.protected_free_bytes,
        )
        .expect("chunk request");
        let mut facts = BTreeMap::new();
        facts.insert(
            row.identity.clone(),
            VolumeFacts {
                identity: row.identity.clone(),
                free_bytes: PROTECTED_FREE_SPACE_FLOOR + 1024,
                allocation_unit: row.allocation_unit,
            },
        );
        let provider = CapacityProvider::qualified_fixture(facts);
        let lease = provider
            .admit(
                &request.plan().expect("plan"),
                &request.inventory().expect("inventory"),
            )
            .expect("admit chunk");
        store
            .persist_chunk_claim_with_lease(&request, &provider, lease)
            .expect("persist chunk claim");
        let issue_id = Uuid::new_v4();
        store
            .issue_chunk_claim(chunk_id, operation_id, issue_id)
            .expect("issue chunk");
        let checkpoint = chunk_staging::ChunkStagingRecord::decode(
            &serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "chunk_id": chunk_id.as_uuid(),
                "operation_id": operation_id,
                "issue_id": issue_id,
                "participant_id": filesystem,
                "output_key": "partial.wav",
                "logical_offset": 0,
                "logical_length": row.logical_bytes,
                "status": "PartialNeedsRetry",
                "staging": {
                    "schema_version": 1,
                    "operation_id": operation_id,
                    "participant_id": filesystem,
                    "effect_id": Uuid::new_v4(),
                    "output_id": Uuid::new_v4(),
                    "staging_root": {
                        "owner": Uuid::new_v4(),
                        "registration_id": Uuid::new_v4(),
                        "generation": 1
                    },
                    "source_root": {
                        "owner": Uuid::new_v4(),
                        "registration_id": Uuid::new_v4(),
                        "generation": 1
                    },
                    "source_fence": {
                        "source_id": source_fence.source_id(),
                        "root_identity": source_fence.root_identity(),
                        "database_identity": source_fence.database_identity(),
                        "lifecycle_generation": source_fence.lifecycle_generation(),
                        "source_revision": source_fence.source_revision(),
                        "lease_epoch": source_fence.lease_epoch()
                    },
                    "locator": "partial.wav",
                    "logical_object_id": Uuid::new_v4(),
                    "physical_object_id": "physical-partial-file",
                    "bytes": 1,
                    "content_hash": vec![7; 32],
                    "request_fingerprint": vec![8; 32]
                }
            }))
            .expect("encode checkpoint fixture"),
        )
        .expect("decode checkpoint fixture");
        store.set_insert_test_hook(|boundary| {
            if boundary == InsertTestBoundary::BeforePrecommitValidation {
                InsertTestDirective::Fail(OperationJournalStoreError::IdentityCheckUnavailable {
                    reason: String::from("forced checkpoint precommit failure"),
                })
            } else {
                InsertTestDirective::Continue
            }
        });
        assert!(store.checkpoint_chunk_staging_record(&checkpoint).is_err());
        assert!(
            store
                .read_chunk_staging_row(chunk_id)
                .expect("read rollback")
                .is_none()
        );
        assert_eq!(provider.claim_count(), 1);

        store.set_insert_test_hook(|boundary| {
            if boundary == InsertTestBoundary::AfterCommit {
                InsertTestDirective::Fail(OperationJournalStoreError::IdentityCheckUnavailable {
                    reason: String::from("forced checkpoint postcommit uncertainty"),
                })
            } else {
                InsertTestDirective::Continue
            }
        });
        assert!(store.checkpoint_chunk_staging_record(&checkpoint).is_err());
        let (sequence, retained) = store
            .read_chunk_staging_row(chunk_id)
            .expect("read uncertain checkpoint")
            .expect("retained checkpoint");
        assert_eq!(retained, checkpoint);
        assert!(matches!(
            store.checkpoint_chunk_staging_record(&checkpoint).expect("exact replay"),
            chunk_staging::ChunkStagingReceipt::Replay { sequence: replayed }
                if replayed == sequence
        ));
        let mut conflicting = serde_json::to_value(&checkpoint).expect("checkpoint JSON");
        conflicting["staging"]["physical_object_id"] = serde_json::json!("other-file");
        let conflicting = chunk_staging::ChunkStagingRecord::decode(
            &serde_json::to_vec(&conflicting).expect("encode conflicting checkpoint"),
        )
        .expect("decode conflicting checkpoint");
        assert!(store.checkpoint_chunk_staging_record(&conflicting).is_err());
        let release_id = Uuid::new_v4();
        assert!(
            store
                .release_chunk_claim(
                    chunk_id,
                    operation_id,
                    Some(ChunkUnusedReleaseAuthorization::for_test(
                        chunk_id,
                        operation_id,
                        issue_id,
                        release_id,
                    )),
                    release_id,
                )
                .is_err()
        );
        assert_eq!(provider.claim_count(), 1);
        assert!(store.prove_clean_handoff_durable().is_err());
        assert_eq!(
            journal_sequence(&store, operation_id),
            operation_receipt.sequence()
        );
        // A separate unused chunk can be released, but a failed release must leave its exact
        // claim charged and retryable while the staged chunk remains protected.
        let unused_chunk = ChunkId::new();
        let unused_request = ChunkClaimRequest::new(
            unused_chunk,
            operation_id,
            AdmissionClaim::new(
                AdmissionClaimId::new(),
                row.identity.clone(),
                row.allocation_class.into(),
                row.allocated_bytes,
            )
            .expect("unused chunk claim"),
            filesystem,
            String::from("unused-partial.wav"),
            1,
            row.logical_bytes,
            row.allocation_unit,
            row.protected_free_bytes,
        )
        .expect("unused chunk request");
        let unused_lease = provider
            .admit(
                &unused_request.plan().expect("unused plan"),
                &unused_request.inventory().expect("unused inventory"),
            )
            .expect("admit unused chunk");
        store
            .persist_chunk_claim_with_lease(&unused_request, &provider, unused_lease)
            .expect("persist unused chunk claim");
        let unused_issue_id = Uuid::new_v4();
        store
            .issue_chunk_claim(unused_chunk, operation_id, unused_issue_id)
            .expect("issue unused chunk");
        let unused_release_id = Uuid::new_v4();
        store.set_release_test_hook(|boundary| {
            if boundary == ReleaseTestBoundary::BeforePrecommitValidation {
                ReleaseTestDirective::Fail(OperationJournalStoreError::IdentityCheckUnavailable {
                    reason: String::from("injected unused chunk release failure"),
                })
            } else {
                ReleaseTestDirective::Continue
            }
        });
        assert!(
            store
                .release_chunk_claim(
                    unused_chunk,
                    operation_id,
                    Some(ChunkUnusedReleaseAuthorization::for_test(
                        unused_chunk,
                        operation_id,
                        unused_issue_id,
                        unused_release_id,
                    )),
                    unused_release_id,
                )
                .is_err()
        );
        assert_eq!(provider.claim_count(), 2);
        assert_eq!(
            store
                .read_chunk_claim(unused_chunk)
                .expect("unused chunk after failed release")
                .expect("unused chunk retained")
                .state(),
            ChunkClaimState::Issued
        );
        drop(store);

        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen journal");
        assert_eq!(
            reopened
                .read_chunk_staging_row(chunk_id)
                .expect("read restarted row"),
            Some((sequence, checkpoint.clone()))
        );
        let restart_snapshot = reopened
            .rebuild_ordinary_claim_snapshot()
            .expect("rebuild exact retained claims");
        assert_eq!(restart_snapshot.claims().len(), 2);
        assert_eq!(
            restart_snapshot.readiness(),
            &CapacityRebuildReadiness::Ready
        );
        let rebuilt = CapacityProvider::unavailable();
        rebuilt
            .install_rebuilt_claims(&restart_snapshot)
            .expect("install exact restart claims");
        assert_eq!(
            rebuilt.outstanding(&row.identity, row.allocation_class.into()),
            2
        );
        let unused_release = reopened
            .release_chunk_claim(
                unused_chunk,
                operation_id,
                Some(ChunkUnusedReleaseAuthorization::for_test(
                    unused_chunk,
                    operation_id,
                    unused_issue_id,
                    unused_release_id,
                )),
                unused_release_id,
            )
            .expect("retry failed unused release");
        assert_eq!(unused_release.release_id(), unused_release_id);
        assert_eq!(
            reopened
                .release_chunk_claim(unused_chunk, operation_id, None, unused_release_id)
                .expect("exact duplicate release"),
            unused_release
        );
        let after_release = reopened
            .rebuild_ordinary_claim_snapshot()
            .expect("rebuild after one release");
        let rebuilt_after_release = CapacityProvider::unavailable();
        rebuilt_after_release
            .install_rebuilt_claims(&after_release)
            .expect("install one-charge restart state");
        assert_eq!(
            rebuilt_after_release.outstanding(&row.identity, row.allocation_class.into()),
            1
        );
        assert!(matches!(
            reopened
                .checkpoint_chunk_staging_record(&checkpoint)
                .expect("restart replay"),
            chunk_staging::ChunkStagingReceipt::Replay { .. }
        ));
        let exhausted_request = ChunkClaimRequest::new(
            ChunkId::new(),
            operation_id,
            AdmissionClaim::new(
                AdmissionClaimId::new(),
                row.identity.clone(),
                row.allocation_class.into(),
                2048,
            )
            .expect("next chunk claim"),
            filesystem,
            String::from("next-partial.wav"),
            1,
            2048,
            row.allocation_unit,
            row.protected_free_bytes,
        )
        .expect("next chunk request");
        assert!(matches!(
            provider.admit(
                &exhausted_request.plan().expect("next plan"),
                &exhausted_request.inventory().expect("next inventory"),
            ),
            Err(CapacityProviderError::DiskPressureRecoveryOnly { .. })
        ));
        // This direct store test does not perform the owner-side provider reconciliation.
        // The fresh rebuilt provider above is the restart accounting assertion.
        assert_eq!(provider.claim_count(), 2);
        let retry = RetryObservation::new(
            1,
            51,
            52,
            RetryLeaseId::new("chunk-capacity-retry").expect("retry lease"),
            53,
            RetryErrorClass::CapacityUnavailable,
            "disk_pressure_recovery_only",
            None,
        )
        .expect("capacity retry");
        let (retry_receipt, status) = reopened
            .record_post_intent_chunk_exhaustion(
                operation_id,
                operation_receipt.sequence(),
                retry.clone(),
            )
            .expect("classify positive incomplete chunk");
        assert_eq!(status, OperationDisposition::PartialNeedsRetry);
        assert_eq!(
            reopened
                .record_post_intent_chunk_exhaustion(
                    operation_id,
                    operation_receipt.sequence(),
                    retry,
                )
                .expect("duplicate exhaustion completion"),
            (retry_receipt, OperationDisposition::PartialNeedsRetry)
        );
        let recovered = reopened
            .scan_recovery_page(None)
            .expect("recover retry row");
        assert_eq!(recovered.entries().len(), 1);
        assert_eq!(
            recovered.entries()[0]
                .status_snapshot()
                .observed()
                .expect("observed capacity status")
                .disposition(),
            OperationDisposition::PartialNeedsRetry
        );
        assert_eq!(
            reopened
                .get(operation_id)
                .expect("read persisted V16 operation")
                .expect("operation retained")
                .disposition,
            OperationDisposition::RetryPending
        );
        let cancel = CancellationRequestObservation::new(54).expect("cancel observation");
        reopened
            .request_cancel_before_publish(operation_id, retry_receipt.sequence(), cancel)
            .expect("cancel operation");
        assert!(
            reopened
                .checkpoint_chunk_staging_record(&checkpoint)
                .is_err()
        );
        assert_eq!(provider.claim_count(), 2);
    }

    #[test]
    fn post_intent_capacity_exhaustion_without_positive_checkpoint_stays_retry_pending() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let (operation_id, intent, universal, args, legacy_plan) = v5_copy_inputs();
        let operation = StoredOperationRecord::new_v16_command(
            operation_id,
            intent,
            universal,
            UniversalCommandArgs::Copy(args),
            FullCapacityPlan::from_legacy(&legacy_plan).expect("full plan"),
            50,
        );
        let inserted = store.insert_intent(&operation).expect("persist intent");
        let retry = RetryObservation::new(
            1,
            51,
            52,
            RetryLeaseId::new("no-staging-capacity-retry").expect("retry lease"),
            53,
            RetryErrorClass::CapacityUnavailable,
            "recovery_reserve_low",
            None,
        )
        .expect("capacity observation");
        let (_, disposition) = store
            .record_post_intent_chunk_exhaustion(operation_id, inserted.sequence(), retry)
            .expect("classify exhausted chunk without staging");
        assert_eq!(disposition, OperationDisposition::RetryPending);
        drop(store);
        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen store");
        assert_eq!(
            reopened
                .scan_recovery_page(None)
                .expect("recover capacity retry")
                .entries()[0]
                .status_snapshot()
                .observed()
                .expect("observed status")
                .disposition(),
            OperationDisposition::RetryPending
        );
    }

    #[test]
    fn v16_copy_retry_and_cancel_preserve_full_plan_through_store_cas_and_restart() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let (operation_id, intent, universal_intent, command_args, legacy_plan) = v5_copy_inputs();
        let full_plan = FullCapacityPlan::from_legacy(&legacy_plan).expect("full capacity plan");
        let record = StoredOperationRecord::new_v16_command(
            operation_id,
            intent.clone(),
            universal_intent.clone(),
            UniversalCommandArgs::Copy(command_args.clone()),
            full_plan.clone(),
            50,
        );
        let inserted = store.insert_intent(&record).expect("persist V16 intent");
        assert_eq!(inserted.sequence(), 1);

        let retry = RetryObservation::new(
            1,
            51,
            52,
            RetryLeaseId::new("v16-retry-lease").expect("retry lease"),
            53,
            RetryErrorClass::TransientContention,
            "writer_busy",
            Some(String::from("V16 retry")),
        )
        .expect("retry observation");
        let retry_receipt = store
            .record_retry_pending(operation_id, inserted.sequence(), retry.clone())
            .expect("persist V16 retry");
        assert_eq!(retry_receipt.sequence(), 2);
        assert_eq!(
            store
                .record_retry_pending(operation_id, inserted.sequence(), retry.clone())
                .expect("replay V16 retry"),
            retry_receipt
        );

        let cancellation = CancellationRequestObservation::new(54).expect("cancel timestamp");
        let cancel_receipt = store
            .request_cancel_before_publish(operation_id, retry_receipt.sequence(), cancellation)
            .expect("persist V16 cancellation");
        assert_eq!(cancel_receipt.sequence(), 3);
        assert_eq!(
            store
                .request_cancel_before_publish(operation_id, inserted.sequence(), cancellation)
                .expect("replay V16 cancellation"),
            cancel_receipt
        );

        let persisted = store
            .get(operation_id)
            .expect("read V16 transition")
            .expect("V16 transition exists");
        assert_eq!(persisted.schema_version, SCHEMA_V16);
        assert_eq!(persisted.intent, intent);
        assert_eq!(persisted.universal_intent, Some(universal_intent));
        assert_eq!(persisted.command_args(), None);
        assert_eq!(
            persisted.universal_command_args(),
            record.universal_command_args()
        );
        assert!(persisted.capacity_plan.is_none());
        assert_eq!(persisted.full_capacity_plan, Some(full_plan.clone()));
        assert_eq!(persisted.retry_observation(), Some(&retry));
        assert_eq!(persisted.cancellation_request(), Some(&cancellation));
        assert_eq!(
            persisted.disposition,
            OperationDisposition::CancelRequestedBeforePublish
        );
        drop(store);

        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen store");
        let restarted = reopened
            .get(operation_id)
            .expect("read restarted V16 transition")
            .expect("restarted V16 transition exists");
        assert_eq!(restarted.schema_version, SCHEMA_V16);
        assert_eq!(restarted.full_capacity_plan, Some(full_plan));
        assert_eq!(restarted.retry_observation(), Some(&retry));
        assert_eq!(restarted.cancellation_request(), Some(&cancellation));
        drop(reopened);
        drop(fixture);
    }

    #[test]
    fn v6_commands_round_trip_through_store_and_retry_cancel_cas() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");

        let mut inserted = Vec::new();
        for command in [
            UniversalCommandKind::Copy,
            UniversalCommandKind::Duplicate,
            UniversalCommandKind::Import,
        ] {
            let (operation_id, intent, universal_intent, command_args, capacity_plan) =
                v6_command_inputs(command);
            let receipt = store
                .persist_command_intent(
                    operation_id,
                    intent,
                    universal_intent,
                    command_args.clone(),
                    capacity_plan,
                    50,
                )
                .expect("persist V6 command");
            let persisted = store
                .get(operation_id)
                .expect("get V6 command")
                .expect("V6 command row");
            assert_eq!(persisted.schema_version, SCHEMA_V6);
            assert_eq!(persisted.universal_command_args(), Some(&command_args));
            assert_eq!(persisted.disposition, OperationDisposition::None);
            inserted.push((operation_id, receipt, command_args));
        }

        let (operation_id, initial, command_args) =
            inserted.pop().expect("import command is available for CAS");
        assert_eq!(initial.sequence(), 3);
        let retry = RetryObservation::new(
            1,
            51,
            52,
            RetryLeaseId::new("retry-lease-v6").expect("retry lease"),
            53,
            RetryErrorClass::TransientContention,
            "writer_busy",
            Some(String::from("source database is busy")),
        )
        .expect("retry observation");
        let expected_retry = retry.clone();
        let retry_receipt = store
            .record_retry_pending(operation_id, initial.sequence(), retry)
            .expect("persist V6 retry");
        assert_eq!(retry_receipt.sequence(), 4);

        let replay = store
            .record_retry_pending(
                operation_id,
                initial.sequence(),
                RetryObservation::new(
                    1,
                    51,
                    52,
                    RetryLeaseId::new("retry-lease-v6").expect("retry lease"),
                    53,
                    RetryErrorClass::TransientContention,
                    "writer_busy",
                    Some(String::from("source database is busy")),
                )
                .expect("same retry observation"),
            )
            .expect("retry replay");
        assert_eq!(replay, retry_receipt);

        let stale_retry = RetryObservation::new(
            2,
            54,
            55,
            RetryLeaseId::new("retry-lease-v6-next").expect("next retry lease"),
            56,
            RetryErrorClass::TransientAvailability,
            "source_unavailable",
            None,
        )
        .expect("next retry observation");
        assert!(matches!(
            store.record_retry_pending(operation_id, initial.sequence(), stale_retry),
            Err(OperationJournalStoreError::StaleSequence {
                operation_id: actual,
                expected: 3,
                actual: 4
            }) if actual == operation_id
        ));
        let after_stale_retry = store
            .get(operation_id)
            .expect("get V6 row after stale retry")
            .expect("V6 row after stale retry");
        assert_eq!(
            journal_sequence(&store, operation_id),
            retry_receipt.sequence()
        );
        assert_eq!(after_stale_retry.retry_observation(), Some(&expected_retry));
        assert_eq!(after_stale_retry.cancellation_request(), None);

        let cancellation = CancellationRequestObservation::new(56).expect("cancel timestamp");
        assert!(matches!(
            store.request_cancel_before_publish(
                operation_id,
                initial.sequence(),
                CancellationRequestObservation::new(55).expect("stale cancellation")
            ),
            Err(OperationJournalStoreError::StaleSequence {
                operation_id: actual,
                expected: 3,
                actual: 4
            }) if actual == operation_id
        ));
        let after_stale_cancellation = store
            .get(operation_id)
            .expect("get V6 row after stale cancellation")
            .expect("V6 row after stale cancellation");
        assert_eq!(
            journal_sequence(&store, operation_id),
            retry_receipt.sequence()
        );
        assert_eq!(
            after_stale_cancellation.retry_observation(),
            Some(&expected_retry)
        );
        assert_eq!(after_stale_cancellation.cancellation_request(), None);
        let cancel_receipt = store
            .request_cancel_before_publish(operation_id, retry_receipt.sequence(), cancellation)
            .expect("persist V6 cancellation");
        assert_eq!(cancel_receipt.sequence(), 5);
        let canceled = store
            .get(operation_id)
            .expect("get V6 cancellation")
            .expect("canceled V6 row");
        assert_eq!(canceled.schema_version, SCHEMA_V6);
        assert_eq!(canceled.universal_command_args(), Some(&command_args));
        assert_eq!(
            canceled.retry_observation().map(RetryObservation::attempt),
            Some(1)
        );
        assert_eq!(
            canceled
                .cancellation_request()
                .expect("cancellation observation")
                .requested_unix_ms(),
            56
        );

        let cancel_replay = store
            .request_cancel_before_publish(
                operation_id,
                initial.sequence(),
                CancellationRequestObservation::new(56).expect("same cancellation"),
            )
            .expect("cancellation replay");
        assert_eq!(cancel_replay, cancel_receipt);
        assert_eq!(journal_sequence(&store, operation_id), 5);

        drop(store);
        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen store");
        let restarted = reopened
            .get(operation_id)
            .expect("get restarted V6 command")
            .expect("restarted V6 row");
        assert_eq!(restarted.schema_version, SCHEMA_V6);
        assert_eq!(restarted.universal_command_args(), Some(&command_args));
        assert_eq!(
            restarted.disposition,
            OperationDisposition::CancelRequestedBeforePublish
        );
    }

    #[test]
    fn v7_waveform_edit_retry_cancel_preserve_exact_args_and_cas() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let (operation_id, intent, universal_intent, edit_args, capacity_plan) =
            v7_waveform_edit_inputs();
        let inserted = store
            .persist_waveform_edit_intent(
                operation_id,
                intent,
                universal_intent,
                edit_args.clone(),
                capacity_plan,
                50,
            )
            .expect("persist V7 waveform edit");
        assert_eq!(inserted.sequence(), 1);

        let persisted = store
            .get(operation_id)
            .expect("get V7 waveform edit")
            .expect("V7 waveform edit row");
        assert_eq!(persisted.schema_version, SCHEMA_V7);
        assert_eq!(
            persisted.universal_command_args(),
            Some(&UniversalCommandArgs::WaveformEdit(edit_args.clone()))
        );

        let first_retry = RetryObservation::new(
            1,
            51,
            52,
            RetryLeaseId::new("v7-retry-1").expect("retry lease"),
            53,
            RetryErrorClass::TransientContention,
            "writer_busy",
            Some(String::from("V7 source is busy")),
        )
        .expect("first retry observation");
        let retry_receipt = store
            .record_retry_pending(operation_id, inserted.sequence(), first_retry.clone())
            .expect("persist V7 retry");
        assert_eq!(retry_receipt.sequence(), 2);

        let (second_id, second_intent, second_universal, second_edit, second_capacity) =
            v7_waveform_edit_inputs();
        let second_inserted = store
            .persist_waveform_edit_intent(
                second_id,
                second_intent,
                second_universal,
                second_edit,
                second_capacity,
                60,
            )
            .expect("persist second V7 waveform edit");
        assert_eq!(second_inserted.sequence(), 3);

        let next_retry = RetryObservation::new(
            2,
            54,
            55,
            RetryLeaseId::new("v7-retry-2").expect("next retry lease"),
            56,
            RetryErrorClass::TransientAvailability,
            "source_unavailable",
            None,
        )
        .expect("next retry observation");
        assert!(matches!(
            store.record_retry_pending(operation_id, inserted.sequence(), next_retry.clone()),
            Err(OperationJournalStoreError::StaleSequence {
                operation_id: actual,
                expected: 1,
                actual: 2
            }) if actual == operation_id
        ));
        assert_eq!(journal_sequence(&store, operation_id), 2);

        let next_retry_receipt = store
            .record_retry_pending(operation_id, retry_receipt.sequence(), next_retry.clone())
            .expect("persist second V7 retry");
        assert_eq!(next_retry_receipt.sequence(), 4);
        let cancellation = CancellationRequestObservation::new(57).expect("cancel observation");
        assert!(matches!(
            store.request_cancel_before_publish(
                operation_id,
                retry_receipt.sequence(),
                cancellation
            ),
            Err(OperationJournalStoreError::StaleSequence {
                operation_id: actual,
                expected: 2,
                actual: 4
            }) if actual == operation_id
        ));
        let cancel_receipt = store
            .request_cancel_before_publish(
                operation_id,
                next_retry_receipt.sequence(),
                cancellation,
            )
            .expect("persist V7 cancellation");
        assert_eq!(cancel_receipt.sequence(), 5);

        let canceled = store
            .get(operation_id)
            .expect("get canceled V7 waveform edit")
            .expect("canceled V7 waveform edit row");
        assert_eq!(canceled.schema_version, SCHEMA_V7);
        assert_eq!(
            canceled.universal_command_args(),
            Some(&UniversalCommandArgs::WaveformEdit(edit_args.clone()))
        );
        assert_eq!(canceled.retry_observation(), Some(&next_retry));
        assert_eq!(canceled.cancellation_request(), Some(&cancellation));
        assert_eq!(
            journal_sequence(&store, operation_id),
            cancel_receipt.sequence()
        );

        let replay = store
            .request_cancel_before_publish(
                operation_id,
                inserted.sequence(),
                CancellationRequestObservation::new(57).expect("same cancellation"),
            )
            .expect("replay V7 cancellation");
        assert_eq!(replay, cancel_receipt);
        assert_eq!(journal_sequence(&store, operation_id), 5);

        let differing = CancellationRequestObservation::new(58).expect("different cancellation");
        assert!(matches!(
            store.request_cancel_before_publish(operation_id, inserted.sequence(), differing),
            Err(OperationJournalStoreError::CancellationConflict { operation_id: actual })
                if actual == operation_id
        ));
        assert_eq!(journal_sequence(&store, operation_id), 5);

        drop(store);
        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen store");
        let restarted = reopened
            .get(operation_id)
            .expect("get restarted V7 waveform edit")
            .expect("restarted V7 waveform edit row");
        assert_eq!(restarted.schema_version, SCHEMA_V7);
        assert_eq!(
            restarted.universal_command_args(),
            Some(&UniversalCommandArgs::WaveformEdit(edit_args))
        );
        assert_eq!(restarted.retry_observation(), Some(&next_retry));
        assert_eq!(restarted.cancellation_request(), Some(&cancellation));
    }

    #[test]
    fn v8_namespace_retry_cancel_preserve_args_and_replay_after_restart() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let (operation_id, intent, universal_intent, namespace_args, capacity_plan) =
            v8_namespace_inputs("Rename");
        let inserted = store
            .persist_namespace_command_intent_with_outcome(
                operation_id,
                intent,
                universal_intent,
                namespace_args.clone(),
                capacity_plan,
                50,
            )
            .receipt()
            .expect("persist V8 namespace command");
        assert_eq!(inserted.sequence(), 1);

        let retry = RetryObservation::new(
            1,
            51,
            52,
            RetryLeaseId::new("namespace-retry-1").expect("retry lease"),
            53,
            RetryErrorClass::TransientContention,
            "namespace_busy",
            Some(String::from("same-parent check busy")),
        )
        .expect("retry observation");
        let retry_receipt = store
            .record_retry_pending(operation_id, inserted.sequence(), retry.clone())
            .expect("persist V8 retry");
        assert_eq!(retry_receipt.sequence(), 2);

        let retry_replay = store
            .record_retry_pending(operation_id, inserted.sequence(), retry.clone())
            .expect("replay V8 retry");
        assert_eq!(retry_replay, retry_receipt);
        assert_eq!(journal_sequence(&store, operation_id), 2);

        let changed_retry = RetryObservation::new(
            2,
            54,
            55,
            RetryLeaseId::new("namespace-retry-2").expect("next retry lease"),
            56,
            RetryErrorClass::TransientAvailability,
            "namespace_unavailable",
            None,
        )
        .expect("next retry observation");
        assert!(matches!(
            store.record_retry_pending(operation_id, inserted.sequence(), changed_retry),
            Err(OperationJournalStoreError::StaleSequence {
                operation_id: actual,
                expected: 1,
                actual: 2
            }) if actual == operation_id
        ));
        assert_eq!(journal_sequence(&store, operation_id), 2);

        let cancellation = CancellationRequestObservation::new(57).expect("cancel observation");
        let cancel_receipt = store
            .request_cancel_before_publish(operation_id, retry_receipt.sequence(), cancellation)
            .expect("persist V8 cancellation");
        assert_eq!(cancel_receipt.sequence(), 3);

        let canceled = store
            .get(operation_id)
            .expect("get V8 namespace command")
            .expect("V8 namespace row");
        assert_eq!(canceled.schema_version, SCHEMA_V8);
        assert_eq!(
            canceled.universal_command_args(),
            Some(&UniversalCommandArgs::Namespace(namespace_args.clone()))
        );
        assert_eq!(canceled.retry_observation(), Some(&retry));
        assert_eq!(canceled.cancellation_request(), Some(&cancellation));

        let replay = store
            .request_cancel_before_publish(
                operation_id,
                inserted.sequence(),
                CancellationRequestObservation::new(57).expect("same cancellation"),
            )
            .expect("replay V8 cancellation");
        assert_eq!(replay, cancel_receipt);
        assert_eq!(journal_sequence(&store, operation_id), 3);

        let differing_cancellation =
            CancellationRequestObservation::new(58).expect("different cancel observation");
        assert!(matches!(
            store.request_cancel_before_publish(
                operation_id,
                inserted.sequence(),
                differing_cancellation,
            ),
            Err(OperationJournalStoreError::CancellationConflict { operation_id: actual })
                if actual == operation_id
        ));
        assert_eq!(journal_sequence(&store, operation_id), 3);

        drop(store);
        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen store");
        let restarted = reopened
            .get(operation_id)
            .expect("get restarted V8 namespace command")
            .expect("restarted V8 namespace row");
        assert_eq!(restarted.schema_version, SCHEMA_V8);
        assert_eq!(
            restarted.universal_command_args(),
            Some(&UniversalCommandArgs::Namespace(namespace_args))
        );
        assert_eq!(restarted.retry_observation(), Some(&retry));
        assert_eq!(restarted.cancellation_request(), Some(&cancellation));
    }

    #[test]
    fn v8_namespace_move_and_create_folder_are_persisted_as_distinct_commands() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        for (index, variant) in ["Move", "CreateFolder"].into_iter().enumerate() {
            let (operation_id, intent, universal_intent, namespace_args, capacity_plan) =
                v8_namespace_inputs(variant);
            let receipt = store
                .persist_namespace_command_intent(
                    operation_id,
                    intent,
                    universal_intent,
                    namespace_args.clone(),
                    capacity_plan,
                    60 + index as i64,
                )
                .expect("persist namespace command");
            assert_eq!(receipt.sequence(), (index + 1) as i64);
            let persisted = store
                .get(operation_id)
                .expect("get namespace command")
                .expect("namespace command row");
            assert_eq!(persisted.schema_version, SCHEMA_V8);
            assert_eq!(
                persisted.universal_command_args(),
                Some(&UniversalCommandArgs::Namespace(namespace_args))
            );
        }
    }

    #[test]
    fn status_snapshot_tracks_v5_retry_and_cancel_without_cloning_opaque_data() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let (operation_id, intent, universal_intent, command_args, capacity_plan) =
            v5_copy_inputs();
        let inserted = store
            .persist_copy_intent(
                operation_id,
                intent,
                universal_intent,
                command_args,
                capacity_plan,
                50,
            )
            .expect("persist v5 copy intent");

        let initial = store
            .scan_recovery_page(None)
            .expect("scan initial status")
            .entries()[0]
            .status_snapshot();
        let initial_observed = initial.observed().expect("initial observed status");
        assert_eq!(initial.operation_id(), Some(operation_id));
        assert_eq!(initial.sequence(), inserted.sequence());
        assert_eq!(initial_observed.schema_version(), SCHEMA_V5);
        assert_eq!(initial_observed.phase(), OperationPhase::IntentDurable);
        assert_eq!(initial_observed.disposition(), OperationDisposition::None);
        assert_eq!(
            initial_observed.command_kind(),
            Some(UniversalCommandKind::Copy)
        );
        assert_eq!(initial_observed.label(), Some("copy store test"));
        assert_eq!(
            initial_observed.user_status_key(),
            Some("operation.status.copy")
        );
        assert!(initial_observed.retry().is_none());
        assert!(initial_observed.cancellation_request().is_none());
        assert!(!format!("{initial:?}").contains("payload"));

        let retry = RetryObservation::new(
            1,
            51,
            52,
            RetryLeaseId::new("retry-lease-1").expect("retry lease"),
            53,
            RetryErrorClass::TransientContention,
            "writer_busy",
            Some(String::from("opaque retry context")),
        )
        .expect("retry observation");
        let retry_receipt = store
            .record_retry_pending(operation_id, inserted.sequence(), retry.clone())
            .expect("persist retry");
        let retry_status = store
            .scan_recovery_page(Some(
                RecoveryCursor::new(inserted.sequence()).expect("initial cursor"),
            ))
            .expect("scan retry status")
            .entries()[0]
            .status_snapshot();
        let retry_summary = retry_status
            .observed()
            .and_then(|status| status.retry())
            .expect("retry summary");
        assert_eq!(retry_status.sequence(), retry_receipt.sequence());
        assert_eq!(retry_summary.attempt(), 1);
        assert_eq!(retry_summary.requested_unix_ms(), 51);
        assert_eq!(retry_summary.next_retry_unix_ms(), 52);
        assert_eq!(retry_summary.lease_expiry_unix_ms(), 53);
        assert_eq!(
            retry_summary.error_class(),
            RetryErrorClass::TransientContention
        );
        assert_eq!(retry_summary.diagnostic_code(), "writer_busy");
        assert!(!format!("{retry_status:?}").contains("opaque retry context"));

        let cancellation = CancellationRequestObservation::new(56).expect("cancel timestamp");
        let cancel_receipt = store
            .request_cancel_before_publish(operation_id, retry_receipt.sequence(), cancellation)
            .expect("persist cancellation");
        let cancel_status = store
            .scan_recovery_page(Some(
                RecoveryCursor::new(retry_receipt.sequence()).expect("retry cursor"),
            ))
            .expect("scan cancellation status")
            .entries()[0]
            .status_snapshot();
        let cancel_observed = cancel_status.observed().expect("cancel observed status");
        assert_eq!(cancel_status.sequence(), cancel_receipt.sequence());
        assert_eq!(
            cancel_observed.disposition(),
            OperationDisposition::CancelRequestedBeforePublish
        );
        assert_eq!(cancel_observed.cancellation_request(), Some(cancellation));
        assert_eq!(cancel_observed.retry(), Some(retry_summary));

        // The first snapshot owns only its bounded status fields; later journal updates do not
        // mutate or retroactively upgrade it.
        let initial_observed = initial.observed().expect("initial status remains observed");
        assert_eq!(initial.sequence(), inserted.sequence());
        assert_eq!(initial_observed.disposition(), OperationDisposition::None);
        assert!(initial_observed.retry().is_none());
        assert!(initial_observed.cancellation_request().is_none());
    }

    #[test]
    fn status_snapshot_keeps_legacy_retry_fields_missing() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let mut record = intent_record(OperationId::new());
        record.disposition = OperationDisposition::RetryPending;
        let encoded = encode(&record).expect("encode legacy retry row");
        store
            .connection
            .execute(
                "INSERT INTO operation_journal (operation_id, sequence, record)
                 VALUES (?1, 1, ?2)",
                params![record.operation_id.to_string(), encoded],
            )
            .expect("insert legacy retry row");

        let status = store
            .scan_recovery_page(None)
            .expect("scan legacy retry row")
            .entries()[0]
            .status_snapshot();
        let observed = status.observed().expect("legacy row is consistent");
        assert_eq!(observed.schema_version(), SCHEMA_V1);
        assert_eq!(observed.disposition(), OperationDisposition::RetryPending);
        assert!(observed.retry().is_none());
        assert!(observed.user_status_key().is_none());
    }

    fn journal_sequence(store: &OperationJournalStore, operation_id: OperationId) -> i64 {
        store
            .connection
            .query_row(
                "SELECT sequence FROM operation_journal WHERE operation_id = ?1",
                params![operation_id.to_string()],
                |row| row.get(0),
            )
            .expect("read journal sequence")
    }

    #[test]
    fn cancellation_request_persists_v4_without_execution_authority() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let record = universal_intent_record(OperationId::new());
        let inserted = store.insert_intent(&record).expect("insert v3 intent");
        let observation = CancellationRequestObservation::new(43).expect("request timestamp");

        let receipt = store
            .request_cancel_before_publish(record.operation_id, inserted.sequence(), observation)
            .expect("persist cancellation request");
        assert_eq!(receipt.operation_id(), record.operation_id);
        assert_eq!(receipt.sequence(), 2);
        assert_eq!(
            receipt.encoded_bytes(),
            encode(
                &store
                    .get(record.operation_id)
                    .expect("get canceled row")
                    .unwrap()
            )
            .expect("encode persisted cancellation")
            .len()
        );

        let persisted = store
            .get(record.operation_id)
            .expect("get persisted cancellation")
            .expect("cancellation row");
        assert_eq!(persisted.schema_version, SCHEMA_V4);
        assert_eq!(persisted.phase, OperationPhase::IntentDurable);
        assert_eq!(
            persisted.disposition,
            OperationDisposition::CancelRequestedBeforePublish
        );
        assert_eq!(persisted.cancellation_request(), Some(&observation));
        assert_eq!(persisted.intent, record.intent);
        assert_eq!(persisted.universal_intent, record.universal_intent);
        assert_eq!(persisted.capacity_plan, record.capacity_plan);
        assert_eq!(persisted.payload, record.payload);
        assert_eq!(persisted.created_unix_ms, record.created_unix_ms);
        assert_eq!(persisted.updated_unix_ms, observation.requested_unix_ms());

        assert!(matches!(
            store.insert_intent(&persisted),
            Err(OperationJournalStoreError::InconsistentRecord { reason })
                if reason.contains("None disposition")
        ));

        let page = store
            .scan_recovery_page(None)
            .expect("scan cancellation recovery row");
        assert_eq!(page.entries().len(), 1);
        assert_eq!(page.entries()[0].sequence(), receipt.sequence());
        assert!(matches!(
            page.entries()[0].outcome(),
            RecoveryRowOutcome::Consistent(observed) if observed == &persisted
        ));
    }

    #[test]
    fn cancellation_request_replay_is_idempotent_and_different_request_is_rejected() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let record = universal_intent_record(OperationId::new());
        let inserted = store.insert_intent(&record).expect("insert v3 intent");
        let observation = CancellationRequestObservation::new(43).expect("request timestamp");
        let receipt = store
            .request_cancel_before_publish(record.operation_id, inserted.sequence(), observation)
            .expect("persist cancellation request");

        let replay = store
            .request_cancel_before_publish(record.operation_id, inserted.sequence(), observation)
            .expect("replay exact cancellation request");
        assert_eq!(replay, receipt);

        let different = CancellationRequestObservation::new(44).expect("different timestamp");
        assert!(matches!(
            store.request_cancel_before_publish(record.operation_id, receipt.sequence(), different),
            Err(OperationJournalStoreError::CancellationConflict { operation_id })
                if operation_id == record.operation_id
        ));
        let persisted = store
            .get(record.operation_id)
            .expect("get after replay conflict")
            .expect("cancellation row");
        assert_eq!(persisted.cancellation_request(), Some(&observation));
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT sequence FROM operation_journal WHERE operation_id = ?1",
                    params![record.operation_id.to_string()],
                    |row| row.get::<_, i64>(0),
                )
                .expect("read cancellation sequence"),
            receipt.sequence()
        );
    }

    #[test]
    fn cancellation_request_rejects_stale_sequence_and_legacy_rows_unchanged() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let first = universal_intent_record(OperationId::new());
        let first_receipt = store.insert_intent(&first).expect("insert first v3 intent");
        let second = universal_intent_record(OperationId::new());
        store
            .insert_intent(&second)
            .expect("insert second v3 intent");
        let before: Vec<u8> = store
            .connection
            .query_row(
                "SELECT record FROM operation_journal WHERE operation_id = ?1",
                params![first.operation_id.to_string()],
                |row| row.get(0),
            )
            .expect("read first bytes");
        let observation = CancellationRequestObservation::new(43).expect("request timestamp");
        assert!(matches!(
            store.request_cancel_before_publish(first.operation_id, first_receipt.sequence() + 1, observation),
            Err(OperationJournalStoreError::StaleSequence { operation_id, expected, actual })
                if operation_id == first.operation_id
                    && expected == first_receipt.sequence() + 1
                    && actual == first_receipt.sequence()
        ));
        let after: Vec<u8> = store
            .connection
            .query_row(
                "SELECT record FROM operation_journal WHERE operation_id = ?1",
                params![first.operation_id.to_string()],
                |row| row.get(0),
            )
            .expect("read unchanged first bytes");
        assert_eq!(after, before);

        let legacy = intent_record(OperationId::new());
        let legacy_receipt = store.insert_intent(&legacy).expect("insert legacy intent");
        let legacy_before: Vec<u8> = store
            .connection
            .query_row(
                "SELECT record FROM operation_journal WHERE operation_id = ?1",
                params![legacy.operation_id.to_string()],
                |row| row.get(0),
            )
            .expect("read legacy bytes");
        assert!(matches!(
            store.request_cancel_before_publish(
                legacy.operation_id,
                legacy_receipt.sequence(),
                observation
            ),
            Err(OperationJournalStoreError::UnsupportedCancellationSchema { schema_version: 1 })
        ));
        let legacy_after: Vec<u8> = store
            .connection
            .query_row(
                "SELECT record FROM operation_journal WHERE operation_id = ?1",
                params![legacy.operation_id.to_string()],
                |row| row.get(0),
            )
            .expect("read unchanged legacy bytes");
        assert_eq!(legacy_after, legacy_before);

        let external = external_universal_intent_record(OperationId::new());
        let external_receipt = store
            .insert_intent(&external)
            .expect("insert external intent");
        assert!(matches!(
            store.request_cancel_before_publish(
                external.operation_id,
                external_receipt.sequence(),
                observation
            ),
            Err(OperationJournalStoreError::UnsupportedCancellationActor {
                actor: OperationActor::ExternalFs
            })
        ));
    }

    #[test]
    fn cancellation_request_allocates_global_sequence_after_other_operation() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let first = universal_intent_record(OperationId::new());
        let first_receipt = store.insert_intent(&first).expect("insert first v3 intent");
        let observation = CancellationRequestObservation::new(43).expect("request timestamp");
        let cancellation = store
            .request_cancel_before_publish(
                first.operation_id,
                first_receipt.sequence(),
                observation,
            )
            .expect("persist cancellation request");
        let second = universal_intent_record(OperationId::new());
        let second_receipt = store
            .insert_intent(&second)
            .expect("insert second v3 intent");
        assert_eq!(cancellation.sequence(), 2);
        assert_eq!(second_receipt.sequence(), 3);
    }

    #[test]
    fn cancellation_request_rejects_nonpositive_expected_sequence_without_io() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let observation = CancellationRequestObservation::new(43).expect("request timestamp");
        assert!(matches!(
            store.request_cancel_before_publish(OperationId::new(), 0, observation),
            Err(OperationJournalStoreError::InvalidExpectedSequence { expected: 0 })
        ));
    }

    #[test]
    fn cancellation_request_rejects_timestamp_before_current_update() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let record = universal_intent_record(OperationId::new());
        let inserted = store.insert_intent(&record).expect("insert v3 intent");
        let observation = CancellationRequestObservation::new(record.updated_unix_ms - 1)
            .expect("earlier non-negative timestamp");
        assert!(matches!(
            store.request_cancel_before_publish(record.operation_id, inserted.sequence(), observation),
            Err(OperationJournalStoreError::InvalidCancellationRequest { reason })
                if reason.contains("precedes")
        ));
        assert_eq!(
            store
                .get(record.operation_id)
                .expect("get unchanged intent"),
            Some(record)
        );
    }

    #[test]
    fn cancellation_request_rejects_sequence_exhaustion_without_mutating_row() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let record = universal_intent_record(OperationId::new());
        let inserted = store.insert_intent(&record).expect("insert v3 intent");
        let max_id = OperationId::new();
        let max_record = universal_intent_record(max_id);
        store
            .connection
            .execute(
                "INSERT INTO operation_journal (operation_id, sequence, record)
                 VALUES (?1, ?2, ?3)",
                params![
                    max_id.to_string(),
                    i64::MAX,
                    encode(&max_record).expect("max row bytes")
                ],
            )
            .expect("insert sequence exhaustion fixture");
        let before: Vec<u8> = store
            .connection
            .query_row(
                "SELECT record FROM operation_journal WHERE operation_id = ?1",
                params![record.operation_id.to_string()],
                |row| row.get(0),
            )
            .expect("read row before exhaustion");
        let observation = CancellationRequestObservation::new(43).expect("request timestamp");
        assert!(matches!(
            store.request_cancel_before_publish(
                record.operation_id,
                inserted.sequence(),
                observation
            ),
            Err(OperationJournalStoreError::SequenceExhausted)
        ));
        let after: Vec<u8> = store
            .connection
            .query_row(
                "SELECT record FROM operation_journal WHERE operation_id = ?1",
                params![record.operation_id.to_string()],
                |row| row.get(0),
            )
            .expect("read row after exhaustion");
        assert_eq!(after, before);
    }

    #[test]
    fn retry_observation_rejects_sequence_exhaustion_without_mutating_row() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let (operation_id, intent, universal_intent, command_args, capacity_plan) =
            v5_copy_inputs();
        let inserted = store
            .persist_copy_intent(
                operation_id,
                intent,
                universal_intent,
                command_args,
                capacity_plan,
                1,
            )
            .expect("persist v5 copy intent");
        let max_id = OperationId::new();
        let (_, max_intent, max_universal_intent, max_command_args, max_plan) = v5_copy_inputs();
        let max_record = StoredOperationRecord::new_v5_copy(
            max_id,
            max_intent,
            max_universal_intent,
            max_command_args,
            max_plan,
            1,
        );
        store
            .connection
            .execute(
                "INSERT INTO operation_journal (operation_id, sequence, record)
                 VALUES (?1, ?2, ?3)",
                params![
                    max_id.to_string(),
                    i64::MAX,
                    encode(&max_record).expect("max v5 row bytes")
                ],
            )
            .expect("insert sequence exhaustion fixture");
        let before: Vec<u8> = store
            .connection
            .query_row(
                "SELECT record FROM operation_journal WHERE operation_id = ?1",
                params![operation_id.to_string()],
                |row| row.get(0),
            )
            .expect("read row before exhaustion");
        let observation = RetryObservation::new(
            1,
            2,
            3,
            RetryLeaseId::new("retry-lease").expect("retry lease"),
            4,
            RetryErrorClass::TransientContention,
            "busy",
            None,
        )
        .expect("retry observation");
        assert!(matches!(
            store.record_retry_pending(operation_id, inserted.sequence(), observation),
            Err(OperationJournalStoreError::SequenceExhausted)
        ));
        let after: Vec<u8> = store
            .connection
            .query_row(
                "SELECT record FROM operation_journal WHERE operation_id = ?1",
                params![operation_id.to_string()],
                |row| row.get(0),
            )
            .expect("read unchanged row after exhaustion");
        assert_eq!(after, before);
        assert_eq!(journal_sequence(&store, operation_id), inserted.sequence());
    }

    #[test]
    fn insert_rejects_later_phase_disposition_and_missing_capacity() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");

        let mut later_phase = intent_record(OperationId::new());
        later_phase.phase = OperationPhase::Prepared;
        assert!(matches!(
            store.insert_intent(&later_phase),
            Err(OperationJournalStoreError::NotIntentDurable {
                phase: OperationPhase::Prepared
            })
        ));

        let mut later_disposition = intent_record(OperationId::new());
        later_disposition.disposition = OperationDisposition::RetryPending;
        assert!(matches!(
            store.insert_intent(&later_disposition),
            Err(OperationJournalStoreError::InconsistentRecord { reason })
                if reason.contains("None disposition")
        ));

        let mut no_capacity = intent_record(OperationId::new());
        no_capacity.capacity_plan = None;
        assert!(matches!(
            store.insert_intent(&no_capacity),
            Err(OperationJournalStoreError::MissingCapacityPlan)
        ));
    }

    #[test]
    fn corrupt_sequence_is_not_used_to_admit_another_intent() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let retained = intent_record(OperationId::new());
        store
            .connection
            .execute(
                "INSERT INTO operation_journal VALUES (?1, -1, ?2)",
                params![
                    retained.operation_id.to_string(),
                    encode(&retained).expect("fixture bytes")
                ],
            )
            .expect("corrupt sequence fixture");
        let fresh = intent_record(OperationId::new());
        assert!(matches!(
            store.insert_intent(&fresh),
            Err(OperationJournalStoreError::RequiresAudit { .. })
        ));
        assert!(
            store
                .get(fresh.operation_id)
                .expect("lookup rejected intent")
                .is_none()
        );
    }

    #[test]
    fn unknown_existing_file_is_retained_without_mutation() {
        let fixture = fixture();
        let path = database_path(&fixture.owner);
        fs::create_dir_all(path.parent().expect("journal directory")).expect("journal directory");
        let bytes = b"not a sqlite database".to_vec();
        fs::write(&path, &bytes).expect("write unknown file");
        let error = OperationJournalStore::open(&fixture.owner).expect_err("reject unknown file");
        assert!(matches!(
            error,
            OperationJournalStoreError::RequiresAudit { .. }
        ));
        assert_eq!(fs::read(&path).expect("read retained file"), bytes);
    }

    #[test]
    fn future_sqlite_header_is_retained_without_schema_mutation() {
        let fixture = fixture();
        let path = database_path(&fixture.owner);
        fs::create_dir_all(path.parent().expect("journal directory")).expect("journal directory");
        let mut bytes = vec![0_u8; SQLITE_HEADER_BYTES];
        bytes[..SQLITE_HEADER_MAGIC.len()].copy_from_slice(SQLITE_HEADER_MAGIC);
        bytes[18] = 1;
        bytes[19] = 1;
        bytes[SQLITE_USER_VERSION_OFFSET..SQLITE_USER_VERSION_OFFSET + 4]
            .copy_from_slice(&(JOURNAL_SCHEMA_VERSION + 1).to_be_bytes());
        bytes[SQLITE_APPLICATION_ID_OFFSET..SQLITE_APPLICATION_ID_OFFSET + 4]
            .copy_from_slice(&JOURNAL_APPLICATION_ID.to_be_bytes());
        fs::write(&path, &bytes).expect("write future SQLite header");
        let error = OperationJournalStore::open(&fixture.owner).expect_err("reject future header");
        assert!(matches!(
            error,
            OperationJournalStoreError::RequiresAudit { .. }
        ));
        assert_eq!(fs::read(&path).expect("read retained header"), bytes);
    }

    #[test]
    fn new_database_bootstraps_exact_latest_schema() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open journal store");
        let user_version: i64 = store
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read v2 user version");
        assert_eq!(user_version, i64::from(JOURNAL_SCHEMA_VERSION));
        let objects: Vec<(String, String)> = store
            .connection
            .prepare(
                "SELECT name, type FROM sqlite_master
                 WHERE name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .expect("prepare schema objects")
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("query schema objects")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect schema objects");
        assert_eq!(
            objects,
            vec![
                (String::from(JOURNAL_TABLE_NAME), String::from("table")),
                (
                    String::from(chunk_claims::TABLE_NAME),
                    String::from("table")
                ),
                (
                    String::from(chunk_staging::TABLE_NAME),
                    String::from("table")
                ),
                (
                    String::from(database_attempts::START_TABLE),
                    String::from("table")
                ),
                (
                    String::from(database_attempts::OUTCOME_TABLE),
                    String::from("table")
                ),
                (
                    String::from(database_claims::BINDING_TABLE),
                    String::from("table")
                ),
                (
                    String::from(database_claims::SLICE_TABLE),
                    String::from("table")
                ),
                (
                    String::from(FULL_PROGRESS_TABLE_NAME),
                    String::from("table")
                ),
                (
                    String::from(ordinary_claims::TABLE_NAME),
                    String::from("table")
                ),
                (
                    String::from(ordinary_claims::RELEASE_TABLE_NAME),
                    String::from("table")
                ),
                (
                    String::from(publication_bindings::TABLE_NAME),
                    String::from("table")
                ),
                (
                    String::from(recording_chunks::TABLE_NAME),
                    String::from("table")
                ),
                (
                    String::from(recovery_reserve::CHARGE_TABLE_NAME),
                    String::from("table")
                ),
                (
                    String::from(recovery_reserve_binding::TABLE_NAME),
                    String::from("table")
                ),
                (
                    String::from(recovery_reserve::TRANSITION_TABLE_NAME),
                    String::from("table")
                ),
                (
                    String::from(staging_checkpoints::TABLE_NAME),
                    String::from("table")
                ),
            ]
        );
        let full_progress_rows: i64 = store
            .connection
            .query_row(
                &format!("SELECT COUNT(*) FROM {FULL_PROGRESS_TABLE_NAME}"),
                [],
                |row| row.get(0),
            )
            .expect("count empty full-progress table");
        assert_eq!(full_progress_rows, 0);
    }

    #[test]
    fn existing_v7_journal_adds_empty_staging_checkpoint_table_without_rewriting_progress() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open journal");
        let (initial, _) = full_progress_store_records();
        let operation_id = initial.snapshot().intent().operation_id();
        store
            .persist_full(&initial, None)
            .expect("persist progress");
        let previous = full_progress_raw_row(&store, operation_id);
        drop(store);

        let path = database_path(&fixture.owner);
        let connection = Connection::open(&path).expect("open old schema fixture");
        connection
            .execute_batch(&format!(
                "DROP TABLE operation_journal_publication_issuance; DROP TABLE operation_journal_database_attempt_outcome; DROP TABLE operation_journal_database_attempt; DROP TABLE operation_journal_database_claim_slice; DROP TABLE operation_journal_database_binding; DROP TABLE {}; DROP TABLE {}; DROP TABLE {}; PRAGMA user_version = {};",
                recording_chunks::TABLE_NAME,
                staging_checkpoints::TABLE_NAME,
                chunk_staging::TABLE_NAME,
                CHUNK_CLAIM_SCHEMA_VERSION
            ))
            .expect("make exact v7 schema");
        drop(connection);

        let reopened = OperationJournalStore::open(&fixture.owner).expect("migrate v7 journal");
        assert_eq!(full_progress_raw_row(&reopened, operation_id), previous);
        let version: i64 = reopened
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read migrated version");
        assert_eq!(version, i64::from(JOURNAL_SCHEMA_VERSION));
        let rows: i64 = reopened
            .connection
            .query_row(
                &format!("SELECT COUNT(*) FROM {}", staging_checkpoints::TABLE_NAME),
                [],
                |row| row.get(0),
            )
            .expect("read empty staging checkpoint table");
        assert_eq!(rows, 0);
    }

    #[test]
    fn existing_v8_journal_adds_chunk_staging_without_rewriting_progress() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open journal");
        let (initial, _) = full_progress_store_records();
        let operation_id = initial.snapshot().intent().operation_id();
        store
            .persist_full(&initial, None)
            .expect("persist progress");
        let previous = full_progress_raw_row(&store, operation_id);
        drop(store);

        let connection = Connection::open(database_path(&fixture.owner)).expect("open v8 fixture");
        connection
            .execute_batch(&format!(
                "DROP TABLE operation_journal_publication_issuance; DROP TABLE operation_journal_database_attempt_outcome; DROP TABLE operation_journal_database_attempt; DROP TABLE operation_journal_database_claim_slice; DROP TABLE operation_journal_database_binding; DROP TABLE {}; DROP TABLE {}; PRAGMA user_version = {};",
                recording_chunks::TABLE_NAME,
                chunk_staging::TABLE_NAME,
                STAGING_CHECKPOINT_SCHEMA_VERSION
            ))
            .expect("make exact v8 schema");
        drop(connection);

        let reopened = OperationJournalStore::open(&fixture.owner).expect("migrate v8 journal");
        assert_eq!(full_progress_raw_row(&reopened, operation_id), previous);
        let version: i64 = reopened
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read migrated version");
        assert_eq!(version, i64::from(JOURNAL_SCHEMA_VERSION));
        let rows: i64 = reopened
            .connection
            .query_row(
                &format!("SELECT COUNT(*) FROM {}", chunk_staging::TABLE_NAME),
                [],
                |row| row.get(0),
            )
            .expect("read empty chunk staging table");
        assert_eq!(rows, 0);
    }

    #[test]
    fn empty_journal_has_structured_clean_handoff_zero_proof() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open empty store");
        store
            .prove_clean_handoff_durable()
            .expect("empty journal and recovery history are zero");
    }

    #[test]
    fn existing_v4_database_migrates_empty_ordinary_claim_table_without_rewriting_rows() {
        let fixture = fixture();
        let record = intent_record(OperationId::new());
        let (path, encoded) = write_v4_database(&fixture.owner, &record, 13);
        let store = OperationJournalStore::open(&fixture.owner).expect("migrate v4 store");
        let version: i64 = store
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read migrated v4 version");
        assert_eq!(version, i64::from(JOURNAL_SCHEMA_VERSION));
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT sequence, record FROM operation_journal WHERE operation_id = ?1",
                    params![record.operation_id.to_string()],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
                )
                .expect("read migrated v4 operation"),
            (13, encoded)
        );
        let (_, progressed) = full_progress_store_records();
        let progressed_operation_id = progressed.snapshot().intent().operation_id();
        let progressed_encoded = crate::operation_journal::full_progress_codec::encode(&progressed)
            .expect("encode expected full-progress row");
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT sequence, record FROM operation_journal_full_progress
                     WHERE operation_id = ?1",
                    params![progressed_operation_id.to_string()],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
                )
                .expect("read migrated full-progress row"),
            (17, progressed_encoded)
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    &format!(
                        "SELECT volume_device, profile_identity, root_identity, file_identity,
                                allocation_unit, protected_floor_bytes, namespace_version,
                                namespace_name, owner_generation
                         FROM {}",
                        recovery_reserve_binding::TABLE_NAME
                    ),
                    [],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, i64>(4)?,
                            row.get::<_, i64>(5)?,
                            row.get::<_, i64>(6)?,
                            row.get::<_, String>(7)?,
                            row.get::<_, i64>(8)?,
                        ))
                    },
                )
                .expect("read migrated reserve binding"),
            (
                String::from("7"),
                String::from("fixture-profile"),
                String::from("fixture-root"),
                String::from("fixture-file"),
                4096,
                i64::try_from(PROTECTED_FREE_SPACE_FLOOR).expect("fixture floor"),
                1,
                String::from("recovery-reserve.bin"),
                1,
            )
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    &format!(
                        "SELECT charge_id, volume_device, active_volume_device, operation_id,
                                purpose, budget_bytes, margin_bytes, precharge_capacity_bytes,
                                remaining_bytes, state, transition_sequence
                         FROM {}",
                        recovery_reserve::CHARGE_TABLE_NAME
                    ),
                    [],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, i64>(5)?,
                            row.get::<_, i64>(6)?,
                            row.get::<_, i64>(7)?,
                            row.get::<_, i64>(8)?,
                            row.get::<_, String>(9)?,
                            row.get::<_, i64>(10)?,
                        ))
                    },
                )
                .expect("read migrated recovery charge"),
            (
                Uuid::from_u128(40_001).to_string(),
                String::from("7"),
                String::from("7"),
                record.operation_id.to_string(),
                String::from("OperationRecovery"),
                10,
                1,
                0,
                10,
                String::from("Charged"),
                1,
            )
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    &format!(
                        "SELECT charge_id, transition_id, transition_sequence, kind,
                                amount_bytes, remaining_bytes, state
                         FROM {}",
                        recovery_reserve::TRANSITION_TABLE_NAME
                    ),
                    [],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, i64>(4)?,
                            row.get::<_, i64>(5)?,
                            row.get::<_, String>(6)?,
                        ))
                    },
                )
                .expect("read migrated recovery transition"),
            (
                Uuid::from_u128(40_001).to_string(),
                Uuid::from_u128(40_002).to_string(),
                1,
                String::from("Charged"),
                10,
                10,
                String::from("Charged"),
            )
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    &format!("SELECT COUNT(*) FROM {}", ordinary_claims::TABLE_NAME),
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("count migrated ordinary claims"),
            0
        );
        drop(store);
        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen migrated v5");
        assert!(
            reopened
                .read_ordinary_claim_page(None, MAX_PAGE_ROWS)
                .expect("read empty ordinary-claim page")
                .records()
                .is_empty()
        );
        assert!(path.exists());
    }

    #[test]
    fn existing_v5_database_migrates_release_relation_without_rewriting_claim_rows() {
        let fixture = fixture();
        let record = intent_record(OperationId::new());
        let (path, encoded) = write_v5_database(&fixture.owner, &record, 19);
        let claim_id = Uuid::from_u128(7001);
        let operation_id = record.operation_id.as_uuid();
        let before = Connection::open(&path).expect("open v5 claim fixture");
        before
            .execute(
                &format!(
                    "INSERT INTO {} (claim_id, operation_id, volume_device, allocation_class,
                     allocated_bytes) VALUES (?1, ?2, '1', 'DestinationStaging', 1)",
                    ordinary_claims::TABLE_NAME
                ),
                params![claim_id.to_string(), operation_id.to_string()],
            )
            .expect("insert v5 claim fixture");
        drop(before);
        let store = OperationJournalStore::open(&fixture.owner).expect("migrate v5 store");
        let version: i64 = store
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read migrated v5 version");
        assert_eq!(version, i64::from(JOURNAL_SCHEMA_VERSION));
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT sequence, record FROM operation_journal WHERE operation_id = ?1",
                    params![record.operation_id.to_string()],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
                )
                .expect("read migrated operation"),
            (19, encoded)
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    &format!("SELECT COUNT(*) FROM {}", ordinary_claims::TABLE_NAME),
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("count migrated claim"),
            1
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM {}",
                        ordinary_claims::RELEASE_TABLE_NAME
                    ),
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("count migrated release relation"),
            0
        );
    }

    #[test]
    fn ordinary_claim_pages_are_uuid_keyset_bounded_and_restart_stable() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        for index in 1..=257_u128 {
            insert_ordinary_claim(
                &store,
                Uuid::from_u128(index),
                Uuid::from_u128(10_000 + index),
                "7",
                "DestinationStaging",
                i64::try_from(index).expect("fixture bytes"),
            )
            .expect("insert ordinary claim fixture");
            store
                .connection
                .execute(
                    &format!(
                        "INSERT INTO {} (claim_id, operation_id, release_id)
                         VALUES (?1, ?2, ?3)",
                        ordinary_claims::RELEASE_TABLE_NAME
                    ),
                    params![
                        Uuid::from_u128(index).to_string(),
                        Uuid::from_u128(10_000 + index).to_string(),
                        Uuid::from_u128(20_000 + index).to_string(),
                    ],
                )
                .expect("insert release paging fixture");
        }

        let first = store
            .read_ordinary_claim_page(None, MAX_PAGE_ROWS)
            .expect("read first ordinary-claim page");
        assert_eq!(first.records().len(), MAX_PAGE_ROWS);
        assert_eq!(first.records()[0].claim_id().as_uuid(), Uuid::from_u128(1));
        assert_eq!(
            first.records()[MAX_PAGE_ROWS - 1].claim_id().as_uuid(),
            Uuid::from_u128(256)
        );
        let cursor = first.next_cursor().expect("page has a continuation cursor");
        assert_eq!(cursor.claim_id().as_uuid(), Uuid::from_u128(256));

        let second = store
            .read_ordinary_claim_page(Some(cursor), MAX_PAGE_ROWS)
            .expect("read second ordinary-claim page");
        assert_eq!(second.records().len(), 1);
        assert_eq!(
            second.records()[0].claim_id().as_uuid(),
            Uuid::from_u128(257)
        );
        assert!(second.next_cursor().is_none());
        assert!(
            store
                .read_ordinary_claim_page(
                    Some(OrdinaryClaimCursor::from_claim_id(
                        AdmissionClaimId::from_uuid(Uuid::from_u128(u128::MAX))
                            .expect("non-nil end cursor")
                    )),
                    MAX_PAGE_ROWS,
                )
                .expect("read page beyond end")
                .records()
                .is_empty()
        );
        assert!(
            store
                .read_ordinary_claim_page(None, 0)
                .expect("read explicit empty page")
                .records()
                .is_empty()
        );
        assert!(matches!(
            store.read_ordinary_claim_page(None, MAX_PAGE_ROWS + 1),
            Err(OrdinaryClaimReadError::PageLimitExceeded { .. })
        ));

        drop(store);
        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen store");
        let page = reopened
            .read_ordinary_claim_page(None, 2)
            .expect("read claims after restart");
        assert_eq!(page.records().len(), 2);
        assert_eq!(page.records()[0].claim_id().as_uuid(), Uuid::from_u128(1));
        assert_eq!(
            page.records()[0].operation_id().as_uuid(),
            Uuid::from_u128(10_001)
        );
        let rebuild = reopened
            .rebuild_ordinary_claim_snapshot()
            .expect("rebuild paged ordinary claims and releases");
        assert_eq!(rebuild.claims().len(), 257);
        assert!(matches!(
            rebuild.readiness(),
            CapacityRebuildReadiness::RecoveryOnly { diagnostics }
                if diagnostics.len() == MAX_REBUILD_DIAGNOSTICS
        ));
        assert!(
            rebuild
                .claims()
                .iter()
                .all(|claim| claim.release_id().is_none())
        );
    }

    #[test]
    fn ordinary_claim_read_preserves_orphan_operation_association() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let claim_id = Uuid::from_u128(4_001);
        let orphan_operation = Uuid::from_u128(8_001);
        insert_ordinary_claim(
            &store,
            claim_id,
            orphan_operation,
            "9",
            "OperationRecoveryPayload",
            4_096,
        )
        .expect("insert orphan-associated claim");
        drop(store);

        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen store");
        let page = reopened
            .read_ordinary_claim_page(None, MAX_PAGE_ROWS)
            .expect("read orphan-associated claim");
        assert_eq!(page.records().len(), 1);
        assert_eq!(page.records()[0].claim_id().as_uuid(), claim_id);
        assert_eq!(page.records()[0].operation_id().as_uuid(), orphan_operation);
        assert_eq!(page.records()[0].volume().device, 9);
        assert_eq!(
            page.records()[0].allocation_class(),
            crate::operation_journal::full_capacity::FullCapacityAllocationClass::OperationRecoveryPayload
        );
        assert_eq!(page.records()[0].allocated_bytes(), 4_096);
    }

    #[test]
    fn clean_handoff_proof_requires_exact_release_for_every_claim() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let operation_id = OperationId::new();
        store
            .insert_intent(&intent_record(operation_id))
            .expect("insert point-validation operation");
        let claim_id = Uuid::from_u128(4_050);
        insert_ordinary_claim(
            &store,
            claim_id,
            operation_id.as_uuid(),
            "9",
            "OperationRecoveryPayload",
            4_096,
        )
        .expect("insert retained claim");
        assert!(matches!(
            store.prove_clean_handoff_durable(),
            Err(OperationJournalStoreError::RequiresAudit { .. })
        ));
        store
            .connection
            .execute(
                &format!(
                    "INSERT INTO {} (claim_id, operation_id, release_id)
                     VALUES (?1, ?2, ?3)",
                    ordinary_claims::RELEASE_TABLE_NAME
                ),
                params![
                    claim_id.to_string(),
                    operation_id.to_string(),
                    Uuid::from_u128(4_051).to_string(),
                ],
            )
            .expect("insert exact release");
        store
            .prove_clean_handoff_durable()
            .expect("exactly released claim permits clean proof");
    }

    #[test]
    fn ordinary_claim_rebuild_retains_mismatched_and_orphan_release_evidence() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let operation_id = OperationId::new();
        store
            .insert_intent(&intent_record(operation_id))
            .expect("insert point-validation operation");
        let claim_id = Uuid::from_u128(4_101);
        let orphan_claim_id = Uuid::from_u128(4_102);
        let mismatched_operation_id = OperationId::new();
        insert_ordinary_claim(
            &store,
            claim_id,
            operation_id.as_uuid(),
            "9",
            "OperationRecoveryPayload",
            4_096,
        )
        .expect("insert valid claim");
        store
            .connection
            .execute(
                &format!(
                    "INSERT INTO {} (claim_id, operation_id, release_id)
                     VALUES (?1, ?2, ?3)",
                    ordinary_claims::RELEASE_TABLE_NAME
                ),
                params![
                    claim_id.to_string(),
                    mismatched_operation_id.to_string(),
                    Uuid::from_u128(4_103).to_string(),
                ],
            )
            .expect("insert mismatched release evidence");
        store
            .connection
            .execute_batch("PRAGMA foreign_keys = OFF")
            .expect("disable foreign-key checks for orphan fixture");
        store
            .connection
            .execute(
                &format!(
                    "INSERT INTO {} (claim_id, operation_id, release_id)
                     VALUES (?1, ?2, ?3)",
                    ordinary_claims::RELEASE_TABLE_NAME
                ),
                params![
                    orphan_claim_id.to_string(),
                    OperationId::new().to_string(),
                    Uuid::from_u128(4_104).to_string(),
                ],
            )
            .expect("insert orphan release evidence");
        store
            .connection
            .execute_batch("PRAGMA foreign_keys = ON")
            .expect("restore foreign-key checks");

        let snapshot = store
            .rebuild_ordinary_claim_snapshot()
            .expect("read conservative release snapshot");
        assert_eq!(snapshot.claims().len(), 1);
        assert!(matches!(
            snapshot.readiness(),
            CapacityRebuildReadiness::RecoveryOnly { diagnostics }
                if diagnostics.iter().any(|diagnostic| diagnostic.contains("mismatches claim operation"))
                    && diagnostics.iter().any(|diagnostic| diagnostic.contains("orphan ordinary-claim release"))
        ));
        let claim = &snapshot.claims()[0];
        assert!(claim.audit_pending());
        assert!(claim.release_id().is_none());
        let provider = CapacityProvider::unavailable();
        provider
            .install_rebuilt_claims(&snapshot)
            .expect("retain known ambiguous claim charge");
        assert_eq!(
            provider.outstanding(
                &VolumeIdentity { device: 9 },
                crate::operation_journal::full_capacity::FullCapacityAllocationClass::OperationRecoveryPayload,
            ),
            4_096
        );
        assert!(matches!(
            provider.ledger_dispositions().as_slice(),
            [ProvisionalLeaseDisposition::AuditPending { .. }]
        ));
    }

    #[test]
    fn ordinary_claim_rebuild_rejects_malformed_row_without_mutating_retained_evidence() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let claim_id = Uuid::from_u128(4_201);
        let operation_id = OperationId::new();
        store
            .connection
            .pragma_update(None, "ignore_check_constraints", true)
            .expect("enable malformed-row fixture");
        store
            .connection
            .execute(
                &format!(
                    "INSERT INTO {} (claim_id, operation_id, volume_device, allocation_class,
                     allocated_bytes) VALUES (?1, ?2, '9', 'DestinationStaging', 0)",
                    ordinary_claims::TABLE_NAME
                ),
                params![claim_id.to_string(), operation_id.to_string()],
            )
            .expect("insert malformed claim row");
        store
            .connection
            .pragma_update(None, "ignore_check_constraints", false)
            .expect("restore malformed-row checks");

        let snapshot = store
            .rebuild_ordinary_claim_snapshot()
            .expect("return conservative malformed snapshot");
        assert!(snapshot.claims().is_empty());
        assert!(matches!(
            snapshot.readiness(),
            CapacityRebuildReadiness::ReconciliationRequired { diagnostics }
                if diagnostics.iter().any(|diagnostic| diagnostic.contains("could not be decoded"))
        ));
        let provider = CapacityProvider::unavailable();
        assert_eq!(
            provider.install_rebuilt_claims(&snapshot),
            Err(CapacityProviderError::RebuildNotReady)
        );
        let retained: i64 = store
            .connection
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM {} WHERE claim_id = ?1",
                    ordinary_claims::TABLE_NAME
                ),
                params![claim_id.to_string()],
                |row| row.get(0),
            )
            .expect("count retained malformed row");
        assert_eq!(retained, 1);
    }

    #[test]
    fn ordinary_claim_constraints_reject_nil_duplicate_class_zero_and_overflow_rows() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let operation_id = Uuid::from_u128(5_001);
        let claim_id = Uuid::from_u128(5_002);
        insert_ordinary_claim(&store, claim_id, operation_id, "11", "JournalRecord", 1)
            .expect("insert valid claim");
        assert!(
            insert_ordinary_claim(&store, claim_id, operation_id, "11", "JournalRecord", 1,)
                .is_err(),
            "duplicate claim identity must remain unique"
        );
        assert!(
            insert_ordinary_claim(&store, Uuid::nil(), operation_id, "11", "JournalRecord", 1,)
                .is_err(),
            "nil claim identity must be rejected"
        );
        assert!(
            store
                .connection
                .execute(
                    &format!(
                        "INSERT INTO {table}
                         (claim_id, operation_id, volume_device, allocation_class, allocated_bytes)
                         VALUES (?1, ?2, '11', 'JournalRecord', 1)",
                        table = ordinary_claims::TABLE_NAME
                    ),
                    params![
                        Uuid::from_u128(5_006).to_string().to_uppercase(),
                        operation_id.to_string()
                    ],
                )
                .is_err(),
            "non-canonical claim identity must be rejected"
        );
        assert!(
            insert_ordinary_claim(
                &store,
                Uuid::from_u128(5_003),
                operation_id,
                "11",
                "UnknownClass",
                1,
            )
            .is_err(),
            "unknown allocation class must be rejected"
        );
        assert!(
            insert_ordinary_claim(
                &store,
                Uuid::from_u128(5_004),
                operation_id,
                "11",
                "JournalRecord",
                0,
            )
            .is_err(),
            "zero allocated bytes must be rejected"
        );

        store
            .connection
            .execute_batch("PRAGMA ignore_check_constraints = ON;")
            .expect("enable malformed-row fixture");
        let oversized_claim = "!".repeat(4_096);
        store
            .connection
            .execute(
                &format!(
                    "INSERT INTO {table}
                     (claim_id, operation_id, volume_device, allocation_class, allocated_bytes)
                     VALUES (?1, ?2, '11', 'JournalRecord', 1)",
                    table = ordinary_claims::TABLE_NAME
                ),
                params![&oversized_claim, operation_id.to_string()],
            )
            .expect("insert oversized text fixture");
        store
            .connection
            .execute(
                &format!(
                    "INSERT INTO {table}
                     (claim_id, operation_id, volume_device, allocation_class, allocated_bytes)
                     VALUES (?1, ?2, '11', 'JournalRecord', '9223372036854775808')",
                    table = ordinary_claims::TABLE_NAME
                ),
                params![Uuid::from_u128(5_005).to_string(), operation_id.to_string()],
            )
            .expect("insert out-of-range fixture");
        store
            .connection
            .execute_batch("PRAGMA ignore_check_constraints = OFF;")
            .expect("disable malformed-row fixture");
        assert!(matches!(
            store.read_ordinary_claim_page(None, MAX_PAGE_ROWS),
            Err(OrdinaryClaimReadError::MalformedRow { ref reason, .. })
                if reason.contains("claim_id exceeds 36 bytes")
        ));
        store
            .connection
            .execute(
                &format!(
                    "DELETE FROM {} WHERE claim_id = ?1",
                    ordinary_claims::TABLE_NAME
                ),
                params![&oversized_claim],
            )
            .expect("remove oversized text fixture");
        assert!(matches!(
            store.read_ordinary_claim_page(None, MAX_PAGE_ROWS),
            Err(OrdinaryClaimReadError::MalformedRow { .. })
        ));
    }

    #[test]
    fn existing_v2_database_migrates_recovery_reserve_tables_atomically() {
        let fixture = fixture();
        let record = intent_record(OperationId::new());
        let (path, encoded) = write_v2_database(&fixture.owner, &record, 9);
        let store = OperationJournalStore::open(&fixture.owner).expect("migrate v2 store");
        let version: i64 = store
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read migrated version");
        assert_eq!(version, i64::from(JOURNAL_SCHEMA_VERSION));
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT sequence, record FROM operation_journal WHERE operation_id = ?1",
                    params![record.operation_id.to_string()],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
                )
                .expect("read migrated operation"),
            (9, encoded)
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM {charge_table}",
                        charge_table = recovery_reserve::CHARGE_TABLE_NAME
                    ),
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("count migrated charges"),
            0
        );
        drop(store);
        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen migrated v3");
        assert_eq!(
            reopened
                .get(record.operation_id)
                .expect("read reopened operation")
                .expect("operation remains"),
            record
        );
        assert!(path.exists());
    }

    #[test]
    fn existing_v3_database_migrates_reserve_binding_table_atomically() {
        let fixture = fixture();
        let record = intent_record(OperationId::new());
        let (path, encoded) = write_v3_database(&fixture.owner, &record, 11);
        let store = OperationJournalStore::open(&fixture.owner).expect("migrate v3 store");
        let version: i64 = store
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read migrated v3 version");
        assert_eq!(version, i64::from(JOURNAL_SCHEMA_VERSION));
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT sequence, record FROM operation_journal WHERE operation_id = ?1",
                    params![record.operation_id.to_string()],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
                )
                .expect("read migrated v3 operation"),
            (11, encoded)
        );
        let binding_count: i64 = store
            .connection
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM {}",
                    recovery_reserve_binding::TABLE_NAME
                ),
                [],
                |row| row.get(0),
            )
            .expect("count empty reserve bindings");
        assert_eq!(binding_count, 0);
        drop(store);
        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen migrated v4");
        assert_eq!(
            reopened
                .get(record.operation_id)
                .expect("read reopened operation")
                .expect("operation remains"),
            record
        );
        assert!(path.exists());
    }

    #[test]
    fn malformed_recovery_reserve_row_blocks_reopen_readiness() {
        let fixture = fixture();
        let record = intent_record(OperationId::new());
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        store
            .insert_intent(&record)
            .expect("persist durable operation");
        let charge_id = Uuid::new_v4();
        let transition_id = Uuid::new_v4();
        store
            .connection
            .execute(
                &format!(
                    "INSERT INTO {charge_table}
                     (charge_id, volume_device, active_volume_device, operation_id, purpose,
                      budget_bytes, margin_bytes, precharge_capacity_bytes, remaining_bytes,
                      state, transition_sequence)
                     VALUES (?1, '7', '7', ?2, 'OperationRecovery', 10, 1, 11, 10,
                             'Charged', 1)",
                    charge_table = recovery_reserve::CHARGE_TABLE_NAME
                ),
                params![charge_id.to_string(), record.operation_id.to_string()],
            )
            .expect("insert valid charge fixture");
        store
            .connection
            .execute(
                &format!(
                    "INSERT INTO {transition_table}
                     (charge_id, transition_id, transition_sequence, kind, amount_bytes,
                      remaining_bytes, state)
                     VALUES (?1, ?2, 1, 'Charged', 10, 10, 'Charged')",
                    transition_table = recovery_reserve::TRANSITION_TABLE_NAME
                ),
                params![charge_id.to_string(), transition_id.to_string()],
            )
            .expect("insert valid transition fixture");
        let path = database_path(&fixture.owner);
        drop(store);

        let connection = Connection::open(&path).expect("open raw journal");
        connection
            .pragma_update(None, "ignore_check_constraints", 1_i64)
            .expect("allow malformed fixture");
        connection
            .execute(
                &format!(
                    "UPDATE {charge_table}
                     SET state = 'Unknown' WHERE charge_id = ?1",
                    charge_table = recovery_reserve::CHARGE_TABLE_NAME
                ),
                params![charge_id.to_string()],
            )
            .expect("corrupt retained charge");
        drop(connection);

        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen journal");
        assert!(matches!(
            reopened.recovery_readiness(),
            Err(recovery::RecoveryScanError::RequiresAudit { reason })
                if reason.contains("unsupported state")
        ));
    }

    #[test]
    fn full_progress_insert_get_reopen_and_global_sequence() {
        let fixture = fixture();
        let (initial, progressed) = full_progress_store_records();
        let operation_id = initial.snapshot().intent().operation_id();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");

        assert_eq!(
            store
                .persist_full(&initial, None)
                .expect("insert full progress"),
            FullProgressWriteOutcome::Applied { sequence: 1 }
        );
        let first = store
            .get_full(operation_id)
            .expect("read full progress")
            .expect("full-progress row");
        assert_eq!(first.sequence(), 1);
        assert_eq!(first.record(), &initial);
        let first_rehydrated = first
            .record()
            .snapshot()
            .rehydrate_observation()
            .expect("initial stored snapshot rehydrates");
        let first_recaptured = first_rehydrated
            .capture_full_progress_snapshot(first.record().snapshot().intent())
            .expect("initial rehydrated snapshot captures identically");
        assert_eq!(first_recaptured, *first.record().snapshot());

        let legacy = intent_record(OperationId::new());
        assert_eq!(
            store
                .insert_intent(&legacy)
                .expect("insert legacy row")
                .sequence,
            2
        );
        assert_eq!(
            store
                .persist_full(&progressed, Some(1))
                .expect("update full progress"),
            FullProgressWriteOutcome::Applied { sequence: 3 }
        );
        let updated = store
            .get_full(operation_id)
            .expect("read updated full progress")
            .expect("updated full-progress row");
        assert_eq!(updated.sequence(), 3);
        assert_eq!(updated.record(), &progressed);

        drop(store);
        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen store");
        let persisted = reopened
            .get_full(operation_id)
            .expect("read reopened full progress")
            .expect("reopened full-progress row");
        assert_eq!(persisted.sequence(), 3);
        assert_eq!(persisted.record(), &progressed);
        let rehydrated = persisted
            .record()
            .snapshot()
            .rehydrate_observation()
            .expect("reopened stored snapshot rehydrates");
        let recaptured = rehydrated
            .capture_full_progress_snapshot(persisted.record().snapshot().intent())
            .expect("reopened rehydrated snapshot captures identically");
        assert_eq!(recaptured, *persisted.record().snapshot());
    }

    #[test]
    fn full_progress_replays_exactly_and_rejects_sequence_conflicts() {
        let test_fixture = fixture();
        let (initial, progressed) = full_progress_store_records();
        let operation_id = initial.snapshot().intent().operation_id();
        let store = OperationJournalStore::open(&test_fixture.owner).expect("open store");
        store
            .persist_full(&initial, None)
            .expect("insert initial full progress");

        let before = full_progress_raw_row(&store, operation_id);
        assert_eq!(
            store.persist_full(&initial, None).expect("initial replay"),
            FullProgressWriteOutcome::Replay { sequence: 1 }
        );
        assert_eq!(full_progress_raw_row(&store, operation_id), before);
        assert!(matches!(
            store.persist_full(&initial, Some(2)),
            Err(OperationJournalStoreError::FullProgressSequenceConflict {
                expected: Some(2),
                actual: Some(1),
                ..
            })
        ));
        assert_eq!(full_progress_raw_row(&store, operation_id), before);
        assert!(matches!(
            store
                .persist_full(&initial, Some(0))
                .expect_err("non-positive expectation must conflict"),
            OperationJournalStoreError::FullProgressSequenceConflict {
                operation_id: observed,
                expected: Some(0),
                actual: None,
            } if observed == operation_id
        ));
        assert_eq!(full_progress_raw_row(&store, operation_id), before);
        assert!(matches!(
            store.persist_full(&progressed, None),
            Err(OperationJournalStoreError::FullProgressSequenceConflict {
                expected: None,
                actual: Some(1),
                ..
            })
        ));
        assert_eq!(full_progress_raw_row(&store, operation_id), before);
        assert!(matches!(
            store.persist_full(&progressed, Some(2)),
            Err(OperationJournalStoreError::FullProgressSequenceConflict {
                expected: Some(2),
                actual: Some(1),
                ..
            })
        ));
        assert_eq!(full_progress_raw_row(&store, operation_id), before);
        assert_eq!(
            store
                .persist_full(&progressed, Some(1))
                .expect("apply update"),
            FullProgressWriteOutcome::Applied { sequence: 2 }
        );
        let after_update = full_progress_raw_row(&store, operation_id);
        assert_eq!(after_update.1, 2);
        assert_ne!(after_update.2, before.2);
        assert_eq!(
            store
                .persist_full(&progressed, Some(1))
                .expect("update acknowledgement replay"),
            FullProgressWriteOutcome::Replay { sequence: 2 }
        );
        assert_eq!(full_progress_raw_row(&store, operation_id), after_update);
        assert!(matches!(
            store.persist_full(&initial, Some(1)),
            Err(OperationJournalStoreError::FullProgressSequenceConflict {
                expected: Some(1),
                actual: Some(2),
                ..
            })
        ));
        assert_eq!(full_progress_raw_row(&store, operation_id), after_update);
        assert_eq!(
            store
                .get_full(operation_id)
                .expect("read replayed row")
                .expect("row remains present")
                .sequence(),
            2
        );

        let missing_fixture = fixture();
        let missing_store =
            OperationJournalStore::open(&missing_fixture.owner).expect("open missing store");
        assert!(matches!(
            missing_store.persist_full(&initial, Some(1)),
            Err(OperationJournalStoreError::FullProgressSequenceConflict {
                expected: Some(1),
                actual: None,
                ..
            })
        ));
        let missing_rows: i64 = missing_store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM operation_journal_full_progress",
                [],
                |row| row.get(0),
            )
            .expect("count missing expected rows");
        assert_eq!(missing_rows, 0);
    }

    #[test]
    fn full_progress_precommit_sidecar_failure_rolls_back_without_overwrite() {
        let initial_fixture = fixture();
        let (initial, _) = full_progress_store_records();
        let operation_id = initial.snapshot().intent().operation_id();
        let store = OperationJournalStore::open(&initial_fixture.owner).expect("open store");
        let sidecar = database_path(&initial_fixture.owner).with_file_name("journal.sqlite3-wal");
        let marker = b"unexpected full-progress WAL".to_vec();
        let sidecar_for_hook = sidecar.clone();
        let marker_for_hook = marker.clone();
        store.set_insert_test_hook(move |boundary| {
            if boundary == InsertTestBoundary::BeforePrecommitValidation {
                fs::write(&sidecar_for_hook, &marker_for_hook).expect("inject WAL sidecar");
            }
            InsertTestDirective::Continue
        });

        let error = store
            .persist_full(&initial, None)
            .expect_err("unexpected sidecar must reject full-progress commit");
        assert!(matches!(
            error,
            OperationJournalStoreError::RequiresAudit { reason, .. }
                if reason.contains("sidecar")
        ));
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM operation_journal_full_progress",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("count rolled-back rows"),
            0
        );
        assert_eq!(fs::read(&sidecar).expect("retain injected sidecar"), marker);
        fs::remove_file(sidecar).expect("remove retained test sidecar");
        assert!(
            store
                .get_full(operation_id)
                .expect("read absent row")
                .is_none()
        );

        let update_fixture = fixture();
        let (update_initial, update_progressed) = full_progress_store_records();
        let update_id = update_initial.snapshot().intent().operation_id();
        let update_store =
            OperationJournalStore::open(&update_fixture.owner).expect("open update store");
        update_store
            .persist_full(&update_initial, None)
            .expect("insert update baseline");
        let original_row = full_progress_raw_row(&update_store, update_id);
        let update_sidecar =
            database_path(&update_fixture.owner).with_file_name("journal.sqlite3-wal");
        let update_marker = b"unexpected update WAL".to_vec();
        let update_sidecar_for_hook = update_sidecar.clone();
        let update_marker_for_hook = update_marker.clone();
        update_store.set_insert_test_hook(move |boundary| {
            if boundary == InsertTestBoundary::BeforePrecommitValidation {
                fs::write(&update_sidecar_for_hook, &update_marker_for_hook)
                    .expect("inject update WAL sidecar");
            }
            InsertTestDirective::Continue
        });
        let update_error = update_store
            .persist_full(&update_progressed, Some(1))
            .expect_err("sidecar must reject full-progress update");
        assert!(matches!(
            update_error,
            OperationJournalStoreError::RequiresAudit { reason, .. }
                if reason.contains("sidecar")
        ));
        assert_eq!(
            full_progress_raw_row(&update_store, update_id),
            original_row
        );
        assert_eq!(
            fs::read(&update_sidecar).expect("retain update sidecar evidence"),
            update_marker
        );
        fs::remove_file(update_sidecar).expect("remove retained update sidecar");
    }

    #[test]
    fn full_progress_max_sequence_replays_without_allocation_and_rejects_update() {
        let fixture = fixture();
        let (initial, progressed) = full_progress_store_records();
        let operation_id = initial.snapshot().intent().operation_id();
        let encoded = crate::operation_journal::full_progress_codec::encode(&initial)
            .expect("encode max-sequence fixture");
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        store
            .connection
            .execute(
                "INSERT INTO operation_journal_full_progress (operation_id, sequence, record)
                 VALUES (?1, ?2, ?3)",
                params![operation_id.to_string(), i64::MAX, encoded.clone()],
            )
            .expect("insert max full-progress row");

        assert_eq!(
            store
                .persist_full(&initial, None)
                .expect("max-sequence exact replay"),
            FullProgressWriteOutcome::Replay { sequence: i64::MAX }
        );
        assert!(matches!(
            store.persist_full(&progressed, Some(i64::MAX)),
            Err(OperationJournalStoreError::SequenceExhausted)
        ));
        let retained = full_progress_raw_row(&store, operation_id);
        assert_eq!(retained.1, i64::MAX);
        assert_eq!(retained.2, encoded);
    }

    #[test]
    fn full_progress_collision_and_ambiguous_family_reads_fail_closed() {
        let first_fixture = fixture();
        let (initial, _) = full_progress_store_records();
        let operation_id = initial.snapshot().intent().operation_id();
        let first_store = OperationJournalStore::open(&first_fixture.owner).expect("open store");
        first_store
            .persist_full(&initial, None)
            .expect("insert full-progress row");
        let legacy = intent_record(operation_id);
        assert!(matches!(
            first_store.insert_intent(&legacy),
            Err(OperationJournalStoreError::DuplicateOperation { operation_id: observed })
                if observed == operation_id
        ));
        first_store
            .connection
            .execute(
                "INSERT INTO operation_journal (operation_id, sequence, record)
                 VALUES (?1, ?2, ?3)",
                params![
                    operation_id.to_string(),
                    2_i64,
                    encode(&legacy).expect("encode collision fixture")
                ],
            )
            .expect("create ambiguous family rows");
        assert!(matches!(
            first_store.get_full(operation_id),
            Err(OperationJournalStoreError::RequiresAudit { .. })
        ));
        assert!(matches!(
            first_store.get(operation_id),
            Err(OperationJournalStoreError::RequiresAudit { .. })
        ));
        let ambiguous_full_row = full_progress_raw_row(&first_store, operation_id);
        assert!(matches!(
            first_store.persist_full(&initial, None),
            Err(OperationJournalStoreError::RequiresAudit { .. })
        ));
        assert_eq!(
            full_progress_raw_row(&first_store, operation_id),
            ambiguous_full_row
        );
        let ambiguous_legacy_row: (String, i64, Vec<u8>) = first_store
            .connection
            .query_row(
                "SELECT operation_id, sequence, record FROM operation_journal
                 WHERE operation_id = ?1",
                params![operation_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read ambiguous legacy row");
        let cancellation =
            CancellationRequestObservation::new(1).expect("cancellation mutation lookup fixture");
        assert!(matches!(
            first_store.request_cancel_before_publish(operation_id, 2, cancellation),
            Err(OperationJournalStoreError::RequiresAudit { .. })
        ));
        let retained_legacy_row: (String, i64, Vec<u8>) = first_store
            .connection
            .query_row(
                "SELECT operation_id, sequence, record FROM operation_journal
                 WHERE operation_id = ?1",
                params![operation_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read retained ambiguous legacy row");
        assert_eq!(retained_legacy_row, ambiguous_legacy_row);

        let second_fixture = fixture();
        let second_store = OperationJournalStore::open(&second_fixture.owner).expect("open store");
        let reverse_legacy = intent_record(operation_id);
        second_store
            .insert_intent(&reverse_legacy)
            .expect("insert legacy collision row");
        assert!(matches!(
            second_store.persist_full(&initial, None),
            Err(OperationJournalStoreError::DuplicateOperation { operation_id: observed })
                if observed == operation_id
        ));
    }

    #[test]
    fn full_progress_malformed_payload_is_retained_without_overwrite() {
        let fixture = fixture();
        let (initial, _) = full_progress_store_records();
        let operation_id = initial.snapshot().intent().operation_id();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        store
            .persist_full(&initial, None)
            .expect("insert full-progress row");
        store
            .connection
            .execute(
                "UPDATE operation_journal_full_progress SET record = ?1 WHERE operation_id = ?2",
                params!["retained malformed payload", operation_id.to_string()],
            )
            .expect("corrupt full-progress payload type");

        assert!(matches!(
            store.get_full(operation_id),
            Err(OperationJournalStoreError::RequiresAudit { reason, .. })
                if reason.contains("BLOB")
        ));
        assert!(matches!(
            store.persist_full(&initial, None),
            Err(OperationJournalStoreError::RequiresAudit { .. })
        ));
        let retained: String = store
            .connection
            .query_row(
                "SELECT record FROM operation_journal_full_progress WHERE operation_id = ?1",
                params![operation_id.to_string()],
                |row| row.get(0),
            )
            .expect("read retained malformed payload");
        assert_eq!(retained, "retained malformed payload");
    }

    #[test]
    fn full_progress_malformed_blob_and_sequence_cells_require_audit() {
        let blob_fixture = fixture();
        let (initial, _) = full_progress_store_records();
        let blob_id = initial.snapshot().intent().operation_id();
        let blob_store = OperationJournalStore::open(&blob_fixture.owner).expect("open blob store");
        blob_store
            .connection
            .execute(
                "INSERT INTO operation_journal_full_progress (operation_id, sequence, record)
                 VALUES (?1, ?2, ?3)",
                params![blob_id.to_string(), 1_i64, vec![0x7b_u8, 0x00_u8]],
            )
            .expect("insert malformed JSON blob");
        assert!(matches!(
            blob_store.get_full(blob_id),
            Err(OperationJournalStoreError::RequiresAudit { reason, .. })
                if reason.contains("decode")
        ));
        assert!(matches!(
            blob_store.persist_full(&initial, None),
            Err(OperationJournalStoreError::RequiresAudit { reason, .. })
                if reason.contains("decode")
        ));
        let retained_blob: Vec<u8> = blob_store
            .connection
            .query_row(
                "SELECT record FROM operation_journal_full_progress WHERE operation_id = ?1",
                params![blob_id.to_string()],
                |row| row.get(0),
            )
            .expect("read retained malformed blob");
        assert_eq!(retained_blob, vec![0x7b_u8, 0x00_u8]);

        for (label, sequence_sql, expected_reason) in [
            ("noninteger", "'not-an-integer'", "SQLite integer"),
            ("nonpositive", "-1", "not positive"),
        ] {
            let sequence_fixture = fixture();
            let sequence_store =
                OperationJournalStore::open(&sequence_fixture.owner).expect("open sequence store");
            let sequence_id = initial.snapshot().intent().operation_id();
            let encoded = crate::operation_journal::full_progress_codec::encode(&initial)
                .expect("encode sequence fixture");
            sequence_store
                .connection
                .execute(
                    "INSERT INTO operation_journal_full_progress (operation_id, sequence, record)
                     VALUES (?1, 1, ?2)",
                    params![sequence_id.to_string(), encoded.as_slice()],
                )
                .expect("insert sequence fixture");
            sequence_store
                .connection
                .execute_batch(&format!(
                    "UPDATE operation_journal_full_progress
                     SET sequence = {sequence_sql}
                     WHERE operation_id = '{}';",
                    sequence_id
                ))
                .expect("corrupt sequence cell");
            assert!(
                matches!(
                    sequence_store.get_full(sequence_id),
                    Err(OperationJournalStoreError::RequiresAudit { reason, .. })
                        if reason.contains(expected_reason)
                ),
                "{label} sequence should require audit"
            );
            assert!(
                matches!(
                    sequence_store.persist_full(&initial, None),
                    Err(OperationJournalStoreError::RequiresAudit { reason, .. })
                        if reason.contains(expected_reason)
                ),
                "{label} persist should require audit"
            );
            let retained_sequence: String = sequence_store
                .connection
                .query_row(
                    "SELECT CAST(sequence AS TEXT) FROM operation_journal_full_progress
                     WHERE operation_id = ?1",
                    params![sequence_id.to_string()],
                    |row| row.get(0),
                )
                .expect("read retained sequence cell");
            assert_eq!(retained_sequence, sequence_sql.trim_matches('\''));
        }
    }

    #[test]
    fn full_progress_key_mismatch_and_oversize_payload_require_audit() {
        let mismatch_fixture = fixture();
        let (initial, _) = full_progress_store_records();
        let operation_id = initial.snapshot().intent().operation_id();
        let wrong_key = OperationId::new();
        let encoded = crate::operation_journal::full_progress_codec::encode(&initial)
            .expect("encode key-mismatch fixture");
        let mismatch_store =
            OperationJournalStore::open(&mismatch_fixture.owner).expect("open key-mismatch store");
        mismatch_store
            .connection
            .execute(
                "INSERT INTO operation_journal_full_progress (operation_id, sequence, record)
                 VALUES (?1, ?2, ?3)",
                params![wrong_key.to_string(), 1_i64, encoded.as_slice()],
            )
            .expect("insert key-mismatch row");
        let mut wrong_candidate_value =
            serde_json::to_value(&initial).expect("serialize key-mismatch candidate");
        wrong_candidate_value["snapshot"]["intent"]["operation_id"] =
            serde_json::json!(wrong_key.as_uuid());
        let wrong_candidate: crate::operation_journal::full_progress_codec::StoredFullProgressRecord =
            serde_json::from_value(wrong_candidate_value)
                .expect("decode key-mismatch candidate");
        assert!(matches!(
            mismatch_store.get_full(wrong_key),
            Err(OperationJournalStoreError::RequiresAudit { reason, .. })
                if reason.contains("identity")
        ));
        assert!(matches!(
            mismatch_store.persist_full(&wrong_candidate, None),
            Err(OperationJournalStoreError::RequiresAudit { reason, .. })
                if reason.contains("identity")
        ));
        let retained_mismatch: Vec<u8> = mismatch_store
            .connection
            .query_row(
                "SELECT record FROM operation_journal_full_progress WHERE operation_id = ?1",
                params![wrong_key.to_string()],
                |row| row.get(0),
            )
            .expect("read retained key-mismatch row");
        assert_eq!(retained_mismatch, encoded);
        assert!(
            mismatch_store
                .get_full(operation_id)
                .expect("read missing original key")
                .is_none()
        );

        let oversize_fixture = fixture();
        let oversize_store =
            OperationJournalStore::open(&oversize_fixture.owner).expect("open oversize store");
        oversize_store
            .connection
            .execute_batch("PRAGMA ignore_check_constraints = ON")
            .expect("disable checks for retained corruption fixture");
        oversize_store
            .connection
            .execute(
                "INSERT INTO operation_journal_full_progress (operation_id, sequence, record)
                 VALUES (?1, ?2, zeroblob(?3))",
                params![
                    operation_id.to_string(),
                    1_i64,
                    (crate::operation_journal::full_progress_codec::FULL_PROGRESS_MAX_RECORD_BYTES
                        + 1) as i64
                ],
            )
            .expect("insert oversize retained row");
        oversize_store
            .connection
            .execute_batch("PRAGMA ignore_check_constraints = OFF")
            .expect("restore checks after corruption fixture");
        assert!(matches!(
            oversize_store.get_full(operation_id),
            Err(OperationJournalStoreError::RequiresAudit { reason, .. })
                if reason.contains("exceeds")
        ));
        assert!(matches!(
            oversize_store.persist_full(&initial, None),
            Err(OperationJournalStoreError::RequiresAudit { reason, .. })
                if reason.contains("exceeds")
        ));
        let retained_length: i64 = oversize_store
            .connection
            .query_row(
                "SELECT length(record) FROM operation_journal_full_progress WHERE operation_id = ?1",
                params![operation_id.to_string()],
                |row| row.get(0),
            )
            .expect("read retained oversize length");
        assert_eq!(
            retained_length,
            (crate::operation_journal::full_progress_codec::FULL_PROGRESS_MAX_RECORD_BYTES + 1)
                as i64
        );
    }

    #[test]
    fn full_progress_changed_frozen_intent_conflicts_without_mutation() {
        let fixture = fixture();
        let (initial, _) = full_progress_store_records();
        let operation_id = initial.snapshot().intent().operation_id();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        store
            .persist_full(&initial, None)
            .expect("insert initial full progress");

        let mut changed = serde_json::to_value(&initial).expect("serialize full-progress fixture");
        changed["snapshot"]["intent"]["capacity_plan"]["allocations"][0]["logical_bytes"] =
            serde_json::json!(13);
        changed["snapshot"]["intent"]["capacity_plan"]["allocations"][0]["allocated_bytes"] =
            serde_json::json!(13);
        changed["snapshot"]["intent"]["admission_claims"]["claims"][0]["allocated_bytes"] =
            serde_json::json!(13);
        let changed: crate::operation_journal::full_progress_codec::StoredFullProgressRecord =
            serde_json::from_value(changed).expect("decode changed frozen intent");

        assert!(matches!(
            store.persist_full(&changed, Some(1)),
            Err(OperationJournalStoreError::FullProgressSequenceConflict {
                operation_id: observed,
                expected: Some(1),
                actual: Some(1),
            }) if observed == operation_id
        ));
        let retained = store
            .get_full(operation_id)
            .expect("read retained original")
            .expect("original remains present");
        assert_eq!(retained.sequence(), 1);
        assert_eq!(retained.record(), &initial);
    }

    #[test]
    fn full_progress_duplicate_sequence_is_rejected_by_reads_and_recovery() {
        let fixture = fixture();
        let (initial, _) = full_progress_store_records();
        let operation_id = initial.snapshot().intent().operation_id();
        let legacy = intent_record(OperationId::new());
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        store
            .persist_full(&initial, None)
            .expect("insert full-progress row");
        store
            .connection
            .execute(
                "INSERT INTO operation_journal (operation_id, sequence, record)
                 VALUES (?1, ?2, ?3)",
                params![
                    legacy.operation_id.to_string(),
                    1_i64,
                    encode(&legacy).expect("encode duplicate-sequence fixture")
                ],
            )
            .expect("insert duplicate legacy sequence");

        assert!(matches!(
            store.get_full(operation_id),
            Err(OperationJournalStoreError::RequiresAudit { reason, .. })
                if reason.contains("sequence")
        ));
        assert!(matches!(
            store.get(legacy.operation_id),
            Err(OperationJournalStoreError::RequiresAudit { reason, .. })
                if reason.contains("sequence")
        ));
        assert!(matches!(
            store.scan_recovery_page(None),
            Err(recovery::RecoveryScanError::RequiresAudit { reason })
                if reason.contains("sequence")
        ));
    }

    #[test]
    fn existing_v1_database_migrates_without_rewriting_legacy_rows() {
        let fixture = fixture();
        let record = intent_record(OperationId::new());
        let (path, encoded) = write_v1_database(&fixture.owner, &record, 7);
        let before = Connection::open(&path)
            .expect("open v1 snapshot")
            .query_row(
                "SELECT operation_id, sequence, record FROM operation_journal WHERE operation_id = ?1",
                params![record.operation_id.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, Vec<u8>>(2)?)),
            )
            .expect("read v1 snapshot");

        let observe_configuration = |boundary: MigrationTestBoundary,
                                     connection: &Connection|
         -> Option<OperationJournalStoreError> {
            if boundary == MigrationTestBoundary::BeforeMigration {
                let journal_mode: String = connection
                    .pragma_query_value(None, "journal_mode", |row| row.get(0))
                    .expect("read migration journal mode");
                let locking_mode: String = connection
                    .pragma_query_value(None, "locking_mode", |row| row.get(0))
                    .expect("read migration locking mode");
                let synchronous: i64 = connection
                    .pragma_query_value(None, "synchronous", |row| row.get(0))
                    .expect("read migration synchronous mode");
                let foreign_keys: i64 = connection
                    .pragma_query_value(None, "foreign_keys", |row| row.get(0))
                    .expect("read migration foreign keys");
                assert_eq!(journal_mode, "delete");
                assert_eq!(locking_mode, "exclusive");
                assert_eq!(synchronous, 3);
                assert_eq!(foreign_keys, 1);
            }
            None
        };

        let store = OperationJournalStore::open_inner_with_hooks(
            &fixture.owner,
            OpenHooks {
                before_sqlite_open: None,
                migration_failure: Some(&observe_configuration),
            },
        )
        .expect("migrate v1 store");
        let after = store
            .connection
            .query_row(
                "SELECT operation_id, sequence, record FROM operation_journal WHERE operation_id = ?1",
                params![record.operation_id.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, Vec<u8>>(2)?)),
            )
            .expect("read migrated snapshot");
        assert_eq!(before, after);
        assert_eq!(before.0, record.operation_id.to_string());
        assert_eq!(before.1, 7);
        assert_eq!(before.2, encoded);
        assert_eq!(
            store.get(record.operation_id).expect("read migrated row"),
            Some(record.clone())
        );
        let retained: Vec<u8> = store
            .connection
            .query_row(
                "SELECT record FROM operation_journal WHERE operation_id = ?1",
                params![record.operation_id.to_string()],
                |row| row.get(0),
            )
            .expect("read migrated bytes");
        assert_eq!(retained, encoded);
        let user_version: i64 = store
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read migrated user version");
        assert_eq!(user_version, i64::from(JOURNAL_SCHEMA_VERSION));
        let full_progress_rows: i64 = store
            .connection
            .query_row(
                &format!("SELECT COUNT(*) FROM {FULL_PROGRESS_TABLE_NAME}"),
                [],
                |row| row.get(0),
            )
            .expect("count migrated full-progress rows");
        assert_eq!(full_progress_rows, 0);
        drop(store);

        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen migrated v2");
        let reopened_snapshot = reopened
            .connection
            .query_row(
                "SELECT operation_id, sequence, record FROM operation_journal WHERE operation_id = ?1",
                params![record.operation_id.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, Vec<u8>>(2)?)),
            )
            .expect("read reopened snapshot");
        assert_eq!(reopened_snapshot, before);
        assert_eq!(
            reopened
                .get(record.operation_id)
                .expect("read reopened row"),
            Some(record)
        );
    }

    #[test]
    fn migration_failure_rolls_back_new_table_and_version() {
        let first_fixture = fixture();
        let record = intent_record(OperationId::new());
        let (path, encoded) = write_v1_database(&first_fixture.owner, &record, 11);
        let failure = |boundary, _connection: &Connection| {
            (boundary == MigrationTestBoundary::AfterFullProgressTable).then_some(
                OperationJournalStoreError::IdentityCheckUnavailable {
                    reason: String::from("forced migration failure"),
                },
            )
        };
        let error = OperationJournalStore::open_inner_with_hooks(
            &first_fixture.owner,
            OpenHooks {
                before_sqlite_open: None,
                migration_failure: Some(&failure),
            },
        )
        .expect_err("forced migration failure");
        assert!(matches!(
            error,
            OperationJournalStoreError::IdentityCheckUnavailable { reason }
                if reason == "forced migration failure"
        ));

        let connection = Connection::open(&path).expect("open rolled-back v1 database");
        let user_version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read rolled-back user version");
        assert_eq!(user_version, i64::from(LEGACY_JOURNAL_SCHEMA_VERSION));
        let full_progress_exists: Option<String> = connection
            .query_row(
                "SELECT name FROM sqlite_master WHERE name = ?1",
                params![FULL_PROGRESS_TABLE_NAME],
                |row| row.get(0),
            )
            .optional()
            .expect("query rolled-back table");
        assert_eq!(full_progress_exists, None);
        let retained: Vec<u8> = connection
            .query_row(
                "SELECT record FROM operation_journal WHERE operation_id = ?1",
                params![record.operation_id.to_string()],
                |row| row.get(0),
            )
            .expect("read rolled-back legacy bytes");
        assert_eq!(retained, encoded);
        let retained_snapshot = connection
            .query_row(
                "SELECT operation_id, sequence, record FROM operation_journal WHERE operation_id = ?1",
                params![record.operation_id.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, Vec<u8>>(2)?)),
            )
            .expect("read rolled-back legacy snapshot");
        assert_eq!(
            retained_snapshot,
            (record.operation_id.to_string(), 11, encoded.clone())
        );

        let second_fixture = fixture();
        let second_record = intent_record(OperationId::new());
        let (second_path, second_encoded) =
            write_v1_database(&second_fixture.owner, &second_record, 11);
        let second_failure = |boundary: MigrationTestBoundary, connection: &Connection| {
            if boundary == MigrationTestBoundary::BeforeCommit {
                let version: i64 = connection
                    .pragma_query_value(None, "user_version", |row| row.get(0))
                    .expect("read bumped migration version");
                assert_eq!(version, i64::from(JOURNAL_SCHEMA_VERSION));
                let full_progress_exists: Option<String> = connection
                    .query_row(
                        "SELECT name FROM sqlite_master WHERE name = ?1",
                        params![FULL_PROGRESS_TABLE_NAME],
                        |row| row.get(0),
                    )
                    .optional()
                    .expect("query pre-commit full-progress table");
                assert_eq!(
                    full_progress_exists,
                    Some(String::from(FULL_PROGRESS_TABLE_NAME))
                );
                return Some(OperationJournalStoreError::IdentityCheckUnavailable {
                    reason: String::from("forced pre-commit migration failure"),
                });
            }
            None
        };
        let second_error = OperationJournalStore::open_inner_with_hooks(
            &second_fixture.owner,
            OpenHooks {
                before_sqlite_open: None,
                migration_failure: Some(&second_failure),
            },
        )
        .expect_err("forced pre-commit migration failure");
        assert!(matches!(
            second_error,
            OperationJournalStoreError::IdentityCheckUnavailable { reason }
                if reason == "forced pre-commit migration failure"
        ));
        let second_connection =
            Connection::open(&second_path).expect("open second rolled-back v1 database");
        let second_version: i64 = second_connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read second rolled-back version");
        assert_eq!(second_version, i64::from(LEGACY_JOURNAL_SCHEMA_VERSION));
        let second_full_progress_exists: Option<String> = second_connection
            .query_row(
                "SELECT name FROM sqlite_master WHERE name = ?1",
                params![FULL_PROGRESS_TABLE_NAME],
                |row| row.get(0),
            )
            .optional()
            .expect("query second rolled-back table");
        assert_eq!(second_full_progress_exists, None);
        let second_snapshot = second_connection
            .query_row(
                "SELECT operation_id, sequence, record FROM operation_journal WHERE operation_id = ?1",
                params![second_record.operation_id.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, Vec<u8>>(2)?)),
            )
            .expect("read second rolled-back snapshot");
        assert_eq!(
            second_snapshot,
            (second_record.operation_id.to_string(), 11, second_encoded)
        );
    }

    #[test]
    fn migration_guard_rejects_new_wal_and_shm_before_begin() {
        for suffix in [SQLITE_WAL_SUFFIX, SQLITE_SHM_SUFFIX] {
            let fixture = fixture();
            let record = intent_record(OperationId::new());
            let (path, encoded) = write_v1_database(&fixture.owner, &record, 19);
            let sidecar = path.with_file_name(format!("{JOURNAL_DATABASE_FILE_NAME}{suffix}"));
            let marker = format!("unexpected {suffix}").into_bytes();
            let marker_for_hook = marker.clone();
            let sidecar_for_hook = sidecar.clone();
            let inject_sidecar = move |boundary: MigrationTestBoundary,
                                       _connection: &Connection|
                  -> Option<OperationJournalStoreError> {
                if boundary == MigrationTestBoundary::BeforeMigration {
                    fs::write(&sidecar_for_hook, &marker_for_hook)
                        .expect("install unexpected migration sidecar");
                }
                None
            };
            let error = OperationJournalStore::open_inner_with_hooks(
                &fixture.owner,
                OpenHooks {
                    before_sqlite_open: None,
                    migration_failure: Some(&inject_sidecar),
                },
            )
            .expect_err("reject sidecar introduced before migration");
            assert!(matches!(
                error,
                OperationJournalStoreError::RequiresAudit { reason, .. }
                    if reason.contains("sidecar")
            ));
            let connection = Connection::open(&path).expect("open retained v1 database");
            let version: i64 = connection
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .expect("read retained version");
            assert_eq!(version, i64::from(LEGACY_JOURNAL_SCHEMA_VERSION));
            let full_progress_exists: Option<String> = connection
                .query_row(
                    "SELECT name FROM sqlite_master WHERE name = ?1",
                    params![FULL_PROGRESS_TABLE_NAME],
                    |row| row.get(0),
                )
                .optional()
                .expect("query retained full-progress table");
            assert_eq!(full_progress_exists, None);
            let snapshot = connection
                .query_row(
                    "SELECT operation_id, sequence, record FROM operation_journal WHERE operation_id = ?1",
                    params![record.operation_id.to_string()],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, Vec<u8>>(2)?)),
                )
                .expect("read retained v1 snapshot");
            assert_eq!(
                snapshot,
                (record.operation_id.to_string(), 19, encoded.clone())
            );
            assert_eq!(fs::read(&sidecar).expect("read retained sidecar"), marker);
        }
    }

    #[test]
    fn migration_guard_rejects_sidecar_introduced_after_schema_change_and_rolls_back() {
        let fixture = fixture();
        let record = intent_record(OperationId::new());
        let (path, encoded) = write_v1_database(&fixture.owner, &record, 23);
        let sidecar =
            path.with_file_name(format!("{JOURNAL_DATABASE_FILE_NAME}{SQLITE_WAL_SUFFIX}"));
        let marker = b"unexpected migration WAL".to_vec();
        let marker_for_hook = marker.clone();
        let sidecar_for_hook = sidecar.clone();
        let inject_sidecar = move |boundary: MigrationTestBoundary,
                                   _connection: &Connection|
              -> Option<OperationJournalStoreError> {
            if boundary == MigrationTestBoundary::AfterFullProgressTable {
                fs::write(&sidecar_for_hook, &marker_for_hook)
                    .expect("install in-transaction migration sidecar");
            }
            None
        };
        let error = OperationJournalStore::open_inner_with_hooks(
            &fixture.owner,
            OpenHooks {
                before_sqlite_open: None,
                migration_failure: Some(&inject_sidecar),
            },
        )
        .expect_err("reject sidecar introduced during migration");
        assert!(matches!(
            error,
            OperationJournalStoreError::RequiresAudit { reason, .. }
                if reason.contains("sidecar")
        ));
        let connection = Connection::open(&path).expect("open rolled-back v1 database");
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read rolled-back version");
        assert_eq!(version, i64::from(LEGACY_JOURNAL_SCHEMA_VERSION));
        let full_progress_exists: Option<String> = connection
            .query_row(
                "SELECT name FROM sqlite_master WHERE name = ?1",
                params![FULL_PROGRESS_TABLE_NAME],
                |row| row.get(0),
            )
            .optional()
            .expect("query rolled-back full-progress table");
        assert_eq!(full_progress_exists, None);
        let snapshot = connection
            .query_row(
                "SELECT operation_id, sequence, record FROM operation_journal WHERE operation_id = ?1",
                params![record.operation_id.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, Vec<u8>>(2)?)),
            )
            .expect("read rolled-back v1 snapshot");
        assert_eq!(snapshot, (record.operation_id.to_string(), 23, encoded));
        assert_eq!(
            fs::read(&sidecar).expect("read retained migration sidecar"),
            marker
        );
    }

    #[test]
    fn v1_database_with_extra_full_progress_table_is_rejected_unchanged() {
        let fixture = fixture();
        let record = intent_record(OperationId::new());
        let (path, encoded) = write_v1_database(&fixture.owner, &record, 13);
        let connection = Connection::open(&path).expect("open v1 schema");
        connection
            .execute_batch(CREATE_FULL_PROGRESS_TABLE)
            .expect("add unexpected full-progress table");
        drop(connection);

        let error = OperationJournalStore::open(&fixture.owner).expect_err("reject extra table");
        assert!(matches!(
            error,
            OperationJournalStoreError::RequiresAudit { .. }
        ));
        let connection = Connection::open(&path).expect("reopen retained schema");
        let user_version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read retained v1 version");
        assert_eq!(user_version, i64::from(LEGACY_JOURNAL_SCHEMA_VERSION));
        let retained: Vec<u8> = connection
            .query_row(
                "SELECT record FROM operation_journal WHERE operation_id = ?1",
                params![record.operation_id.to_string()],
                |row| row.get(0),
            )
            .expect("read retained legacy row");
        assert_eq!(retained, encoded);
    }

    #[test]
    fn header_and_connection_versions_must_match_before_schema_configuration() {
        let fixture = fixture();
        let record = intent_record(OperationId::new());
        let (path, encoded) = write_v1_database(&fixture.owner, &record, 17);
        let gate = |path: &Path| {
            let mut bytes = fs::read(path).expect("read v1 header");
            bytes[SQLITE_USER_VERSION_OFFSET..SQLITE_USER_VERSION_OFFSET + 4]
                .copy_from_slice(&JOURNAL_SCHEMA_VERSION.to_be_bytes());
            fs::write(path, bytes).expect("rewrite mismatched header");
        };
        let error = OperationJournalStore::open_inner(&fixture.owner, Some(&gate))
            .expect_err("reject header/PRAGMA mismatch");
        assert!(
            matches!(error, OperationJournalStoreError::RequiresAudit { reason, .. } if reason.contains("differs from PRAGMA"))
        );
        let connection = Connection::open(&path).expect("open retained mismatch");
        let retained: Vec<u8> = connection
            .query_row(
                "SELECT record FROM operation_journal WHERE operation_id = ?1",
                params![record.operation_id.to_string()],
                |row| row.get(0),
            )
            .expect("read retained mismatch row");
        assert_eq!(retained, encoded);
    }

    #[test]
    fn full_progress_sequence_participates_in_legacy_allocation() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open v2 store");
        let full_progress_id = OperationId::new();
        store
            .connection
            .execute(
                &format!(
                    "INSERT INTO {FULL_PROGRESS_TABLE_NAME} (operation_id, sequence, record)
                     VALUES (?1, ?2, ?3)"
                ),
                params![full_progress_id.to_string(), 41_i64, vec![1_u8]],
            )
            .expect("insert full-progress sequence fixture");
        let record = intent_record(OperationId::new());
        let receipt = store
            .insert_intent(&record)
            .expect("allocate after full-progress row");
        assert_eq!(receipt.sequence(), 42);
    }

    #[test]
    fn full_progress_sequence_participates_in_legacy_update_allocation() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open v2 store");
        let record = universal_intent_record(OperationId::new());
        let inserted = store.insert_intent(&record).expect("insert v3 intent");
        let full_progress_id = OperationId::new();
        store
            .connection
            .execute(
                &format!(
                    "INSERT INTO {FULL_PROGRESS_TABLE_NAME} (operation_id, sequence, record)
                     VALUES (?1, ?2, ?3)"
                ),
                params![full_progress_id.to_string(), 41_i64, vec![1_u8]],
            )
            .expect("insert full-progress sequence fixture");
        let observation = CancellationRequestObservation::new(43).expect("request timestamp");
        let cancellation = store
            .request_cancel_before_publish(record.operation_id, inserted.sequence(), observation)
            .expect("allocate update after full-progress row");
        assert_eq!(cancellation.sequence(), 42);
    }

    #[test]
    fn full_progress_sequence_overflow_rejects_legacy_insert() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open v2 store");
        let full_progress_id = OperationId::new();
        store
            .connection
            .execute(
                &format!(
                    "INSERT INTO {FULL_PROGRESS_TABLE_NAME} (operation_id, sequence, record)
                     VALUES (?1, ?2, ?3)"
                ),
                params![full_progress_id.to_string(), i64::MAX, vec![1_u8]],
            )
            .expect("insert max full-progress sequence fixture");
        let record = intent_record(OperationId::new());
        assert!(matches!(
            store.insert_intent(&record),
            Err(OperationJournalStoreError::SequenceExhausted)
        ));
        assert!(
            store
                .get(record.operation_id)
                .expect("read absent overflow row")
                .is_none()
        );
    }

    #[test]
    fn malformed_row_is_retained_and_reported_without_unbounded_read() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let path = database_path(&fixture.owner);
        drop(store);

        let operation_id = OperationId::new();
        let malformed = b"not-json";
        let connection = rusqlite::Connection::open(&path).expect("open journal fixture");
        connection
            .execute(
                "INSERT INTO operation_journal (operation_id, sequence, record)
                 VALUES (?1, 1, ?2)",
                params![operation_id.to_string(), malformed],
            )
            .expect("insert malformed retained row");
        drop(connection);

        let store = OperationJournalStore::open(&fixture.owner).expect("reopen store");
        let error = store
            .get(operation_id)
            .expect_err("report malformed retained row");
        assert!(matches!(
            error,
            OperationJournalStoreError::RetainedRecord {
                operation_id: Some(id), ..
            } if id == operation_id
        ));
        drop(store);

        let connection = rusqlite::Connection::open(&path).expect("reopen retained journal");
        let retained: Vec<u8> = connection
            .query_row(
                "SELECT record FROM operation_journal WHERE operation_id = ?1",
                params![operation_id.to_string()],
                |row| row.get(0),
            )
            .expect("read retained malformed row");
        assert_eq!(retained, malformed);
    }

    #[cfg(unix)]
    #[test]
    fn sidecar_symlink_is_rejected_before_database_creation() {
        let fixture = fixture();
        let root = fixture
            .owner
            .profile_root()
            .join(JOURNAL_DATABASE_ROOT_NAME);
        fs::create_dir_all(&root).expect("journal directory");
        let target = root.join("sidecar-target");
        fs::write(&target, b"retained").expect("sidecar target");
        std::os::unix::fs::symlink(&target, root.join("journal.sqlite3-wal"))
            .expect("sidecar symlink");
        let error =
            OperationJournalStore::open(&fixture.owner).expect_err("reject sidecar symlink");
        assert!(matches!(
            error,
            OperationJournalStoreError::RequiresAudit { .. }
        ));
        assert!(!root.join(JOURNAL_DATABASE_FILE_NAME).exists());
    }

    #[cfg(unix)]
    #[test]
    fn database_replacement_is_rejected_before_lookup_effects() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let path = database_path(&fixture.owner);
        let displaced = path.with_extension("sqlite3.displaced");
        fs::rename(&path, &displaced).expect("displace database");
        fs::write(&path, b"replacement").expect("write replacement database");
        let error = store
            .get(OperationId::new())
            .expect_err("reject replacement");
        assert!(matches!(
            error,
            OperationJournalStoreError::DatabaseReplaced { .. }
        ));
        assert_eq!(fs::read(&path).expect("read replacement"), b"replacement");
        drop(store);
    }

    #[cfg(unix)]
    #[test]
    fn database_replacement_is_rejected_before_insert_effects() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let path = database_path(&fixture.owner);
        let displaced = path.with_extension("sqlite3.displaced");
        fs::rename(&path, &displaced).expect("displace database");
        fs::write(&path, b"replacement").expect("write replacement database");
        let record = intent_record(OperationId::new());
        let error = store
            .insert_intent(&record)
            .expect_err("reject insert into replacement");
        assert!(matches!(
            error,
            OperationJournalStoreError::DatabaseReplaced { .. }
        ));
        assert_eq!(fs::read(&path).expect("read replacement"), b"replacement");
        drop(store);
    }

    #[cfg(unix)]
    #[test]
    fn database_replacement_is_rejected_before_cancellation_effects() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let record = universal_intent_record(OperationId::new());
        let inserted = store.insert_intent(&record).expect("insert v3 intent");
        let path = database_path(&fixture.owner);
        let displaced = path.with_extension("sqlite3.cancel-displaced");
        fs::rename(&path, &displaced).expect("displace database");
        let replacement = b"replacement must remain untouched";
        fs::write(&path, replacement).expect("write replacement database");
        let observation = CancellationRequestObservation::new(53).expect("request timestamp");
        let error = store
            .request_cancel_before_publish(record.operation_id, inserted.sequence(), observation)
            .expect_err("reject cancellation against replacement");
        assert!(matches!(
            error,
            OperationJournalStoreError::DatabaseReplaced { .. }
        ));
        assert_eq!(fs::read(&path).expect("read replacement"), replacement);
        drop(store);
        fs::remove_file(&path).expect("remove replacement");
        fs::rename(displaced, path).expect("restore original database");
    }

    #[cfg(unix)]
    #[test]
    fn replacement_between_creation_and_sqlite_open_is_not_bootstrapped() {
        let fixture = fixture();
        let replacement = b"replacement must remain untouched";
        let gate = |path: &Path| {
            fs::rename(path, path.with_extension("displaced")).expect("displace creator file");
            fs::write(path, replacement).expect("install replacement");
        };
        let result = OperationJournalStore::open_inner(&fixture.owner, Some(&gate));
        assert!(matches!(
            result,
            Err(OperationJournalStoreError::DatabaseReplaced { .. })
        ));
        assert_eq!(
            fs::read(database_path(&fixture.owner)).expect("retained bytes"),
            replacement
        );
    }

    #[cfg(windows)]
    #[test]
    fn sqlite_handle_prevents_database_replacement_while_owned() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let path = database_path(&fixture.owner);
        assert!(fs::rename(&path, path.with_extension("displaced")).is_err());
        let record = intent_record(OperationId::new());
        store
            .insert_intent(&record)
            .expect("original store remains usable");
        assert_eq!(
            store.get(record.operation_id).expect("read original"),
            Some(record)
        );
    }

    #[test]
    fn child_lease_is_retained_by_store() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let result = BoundDatabaseRoot::for_profile_child(
            &fixture.owner,
            Path::new(JOURNAL_DATABASE_ROOT_NAME),
        );
        assert!(matches!(
            result,
            Err(ProfileOwnershipError::ProfileChildAlreadyLeased { .. })
        ));
        drop(store);
        let reopened = BoundDatabaseRoot::for_profile_child(
            &fixture.owner,
            Path::new(JOURNAL_DATABASE_ROOT_NAME),
        )
        .expect("child lease released after store drop");
        drop(reopened);
    }

    #[test]
    fn post_open_wal_sidecar_is_rejected_before_lookup() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let root = fixture
            .owner
            .profile_root()
            .join(JOURNAL_DATABASE_ROOT_NAME);
        fs::write(root.join("journal.sqlite3-wal"), b"unexpected").expect("write WAL sidecar");
        let error = store
            .get(OperationId::new())
            .expect_err("reject WAL sidecar");
        assert!(matches!(
            error,
            OperationJournalStoreError::RequiresAudit { .. }
        ));
        drop(store);
    }

    #[cfg(unix)]
    #[test]
    fn post_open_rollback_symlink_blocks_insert_without_touching_target() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let path = database_path(&fixture.owner);
        let sidecar = path.with_file_name("journal.sqlite3-journal");
        if sidecar.exists() {
            fs::rename(&sidecar, sidecar.with_extension("retained")).expect("retain own journal");
        }
        let target = path.with_file_name("unrelated-file");
        fs::write(&target, b"untouched").expect("target fixture");
        std::os::unix::fs::symlink(&target, &sidecar).expect("replace sidecar with symlink");
        assert!(matches!(
            store.insert_intent(&intent_record(OperationId::new())),
            Err(OperationJournalStoreError::RequiresAudit { .. })
        ));
        assert_eq!(fs::read(&target).expect("target unchanged"), b"untouched");
        fs::remove_file(sidecar).expect("remove only fixture symlink before SQLite closes");
    }

    #[test]
    fn inactive_rollback_sidecar_variants_allow_restart() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let record = intent_record(OperationId::new());
        store.insert_intent(&record).expect("persist intent");
        drop(store);

        let sidecar = database_path(&fixture.owner).with_file_name("journal.sqlite3-journal");
        for contents in [Vec::new(), vec![0_u8; SQLITE_ROLLBACK_HEADER_BYTES]] {
            fs::write(&sidecar, &contents).expect("write inactive rollback sidecar");
            let reopened = OperationJournalStore::open(&fixture.owner)
                .expect("inactive rollback sidecar is safe to reopen");
            assert_eq!(
                reopened
                    .get(record.operation_id)
                    .expect("read persisted intent"),
                Some(record.clone())
            );
            drop(reopened);
            if sidecar.exists() {
                fs::remove_file(&sidecar).expect("remove inactive rollback sidecar");
            }
        }
    }

    #[test]
    fn ambiguous_rollback_sidecars_are_retained_unchanged() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        drop(store);
        let sidecar = database_path(&fixture.owner).with_file_name("journal.sqlite3-journal");
        let cases = [vec![0_u8; SQLITE_ROLLBACK_HEADER_BYTES - 1], {
            let mut bytes = vec![0_u8; SQLITE_ROLLBACK_HEADER_BYTES];
            bytes[0] = 1;
            bytes
        }];
        for contents in cases {
            fs::write(&sidecar, &contents).expect("write ambiguous rollback sidecar");
            let error = OperationJournalStore::open(&fixture.owner)
                .expect_err("ambiguous rollback sidecar requires audit");
            assert!(matches!(
                error,
                OperationJournalStoreError::RequiresAudit { .. }
            ));
            assert_eq!(fs::read(&sidecar).expect("read retained sidecar"), contents);
            fs::remove_file(&sidecar).expect("remove retained sidecar fixture");
        }
    }

    #[cfg(unix)]
    #[test]
    fn rollback_sidecar_replacement_between_validation_and_open_is_rejected() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        drop(store);
        let sidecar = database_path(&fixture.owner).with_file_name("journal.sqlite3-journal");
        fs::write(&sidecar, vec![0_u8; SQLITE_ROLLBACK_HEADER_BYTES])
            .expect("write inactive rollback sidecar");
        let replacement = vec![0_u8; SQLITE_ROLLBACK_HEADER_BYTES];
        let gate = |path: &Path| {
            let sidecar = path.with_file_name("journal.sqlite3-journal");
            fs::rename(&sidecar, sidecar.with_extension("displaced"))
                .expect("displace validated sidecar");
            fs::write(&sidecar, &replacement).expect("install replacement sidecar");
        };
        let error = OperationJournalStore::open_inner(&fixture.owner, Some(&gate))
            .expect_err("replaced rollback sidecar requires audit");
        assert!(matches!(
            error,
            OperationJournalStoreError::RequiresAudit { reason, .. }
                if reason.contains("sidecar") && reason.contains("replaced")
        ));
        assert_eq!(
            fs::read(&sidecar).expect("read retained replacement"),
            replacement
        );
    }

    #[cfg(unix)]
    #[test]
    fn rollback_sidecar_header_change_on_same_file_is_rejected_before_sqlite_open() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        drop(store);
        let database = database_path(&fixture.owner);
        let before_database = fs::read(&database).expect("read original database");
        let sidecar = database.with_file_name("journal.sqlite3-journal");
        fs::write(&sidecar, vec![0_u8; SQLITE_ROLLBACK_HEADER_BYTES])
            .expect("write inactive rollback sidecar");
        let gate = |path: &Path| {
            let sidecar = path.with_file_name("journal.sqlite3-journal");
            let mut file = fs::OpenOptions::new()
                .write(true)
                .open(&sidecar)
                .expect("open sidecar without replacement");
            file.write_all(&[1]).expect("make sidecar header active");
        };
        let error = OperationJournalStore::open_inner(&fixture.owner, Some(&gate))
            .expect_err("same-file active rollback sidecar requires audit");
        assert!(matches!(
            error,
            OperationJournalStoreError::RequiresAudit { reason, .. }
                if reason.contains("active") || reason.contains("ambiguous")
        ));
        assert_eq!(
            fs::read(&database).expect("read retained database"),
            before_database
        );
        let mut expected_sidecar = vec![0_u8; SQLITE_ROLLBACK_HEADER_BYTES];
        expected_sidecar[0] = 1;
        assert_eq!(
            fs::read(&sidecar).expect("read retained sidecar"),
            expected_sidecar
        );
    }

    #[cfg(unix)]
    #[test]
    fn newly_appearing_rollback_sidecar_is_rejected_before_sqlite_open() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        drop(store);
        let database = database_path(&fixture.owner);
        let before_database = fs::read(&database).expect("read original database");
        let sidecar = database.with_file_name("journal.sqlite3-journal");
        let replacement = vec![1_u8; SQLITE_ROLLBACK_HEADER_BYTES];
        let gate = |path: &Path| {
            fs::write(path.with_file_name("journal.sqlite3-journal"), &replacement)
                .expect("install newly appearing sidecar");
        };
        let error = OperationJournalStore::open_inner(&fixture.owner, Some(&gate))
            .expect_err("newly appearing rollback sidecar requires audit");
        assert!(matches!(
            error,
            OperationJournalStoreError::RequiresAudit { .. }
        ));
        assert_eq!(
            fs::read(&database).expect("read retained database"),
            before_database
        );
        assert_eq!(
            fs::read(&sidecar).expect("read retained sidecar"),
            replacement
        );
    }

    #[test]
    fn oversized_retained_blob_is_rejected_before_materializing_record() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let id = OperationId::new();
        // Simulate a corrupt/older producer that bypassed the schema's size constraint.
        store
            .connection
            .pragma_update(None, "ignore_check_constraints", true)
            .expect("fixture bypass");
        store
            .connection
            .execute(
                "INSERT INTO operation_journal VALUES (?1, 1, zeroblob(?2))",
                params![id.to_string(), (MAX_RECORD_BYTES + 1) as i64],
            )
            .expect("oversized fixture");
        store
            .connection
            .pragma_update(None, "ignore_check_constraints", false)
            .expect("restore constraint");
        assert!(
            matches!(store.get(id), Err(OperationJournalStoreError::RetainedRecord { reason, .. }) if reason.contains("length"))
        );
        let retained: i64 = store
            .connection
            .query_row(
                "SELECT length(record) FROM operation_journal WHERE operation_id = ?1",
                params![id.to_string()],
                |row| row.get(0),
            )
            .expect("retained blob length");
        assert_eq!(retained, (MAX_RECORD_BYTES + 1) as i64);
    }

    #[cfg(unix)]
    #[test]
    fn profile_root_replacement_is_rejected_before_lookup_effects() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let root = fixture.owner.profile_root().to_path_buf();
        let displaced = root.with_file_name("journal-store-displaced-root");
        fs::rename(&root, &displaced).expect("displace profile root");
        fs::create_dir(&root).expect("replacement profile root");
        fs::create_dir(root.join(JOURNAL_DATABASE_ROOT_NAME)).expect("replacement journal root");
        let error = store
            .get(OperationId::new())
            .expect_err("reject root replacement");
        assert!(matches!(
            error,
            OperationJournalStoreError::ProfileOwnership(
                ProfileOwnershipError::ProfileRootReplaced { .. }
            )
        ));
        drop(store);
        fs::remove_dir_all(&root).expect("remove replacement root");
        fs::rename(displaced, root).expect("restore profile root");
    }

    #[test]
    fn initial_claim_settlement_commits_exact_multi_volume_claims_and_retains_charge() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let operation_id = OperationId::new();
        let mut record = intent_record(operation_id);
        record
            .capacity_plan
            .as_mut()
            .expect("intent capacity plan")
            .volumes
            .push(DurableVolumeCapacity {
                identity: VolumeIdentity { device: 2 },
                allocation_unit: 1,
                allocation_class: CapacityAllocationClass::JournalRecord,
                logical_bytes: 1,
                allocated_bytes: 1,
                protected_free_bytes: PROTECTED_FREE_SPACE_FLOOR,
            });
        let claims = claims_for_intent(&record);
        let expected_claim_ids = claims
            .claims()
            .iter()
            .map(AdmissionClaim::claim_id)
            .collect::<Vec<_>>();
        let provider = provider_for_intent(&record);

        let settlement = store
            .settle_initial_claims(&record, &provider, &claims)
            .expect("settle initial claims");
        assert!(settlement.outcome().receipt().is_some());
        match settlement
            .lease()
            .expect("retained durable lease")
            .disposition()
        {
            ProvisionalLeaseDisposition::Durable {
                operation_id: retained_operation,
                claim_ids,
            } => {
                assert_eq!(*retained_operation, operation_id);
                assert_eq!(claim_ids, &expected_claim_ids);
            }
            disposition => panic!("unexpected lease disposition: {disposition:?}"),
        }
        assert_eq!(
            store.get(operation_id).expect("read committed record"),
            Some(record)
        );
        assert_eq!(ordinary_claim_count(&store), 2);
        let (outcome, lease) = settlement.into_parts();
        assert!(outcome.receipt().is_some());
        drop(lease);
        assert_eq!(
            provider.outstanding(
                &VolumeIdentity { device: 1 },
                crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging,
            ),
            1
        );
        assert_eq!(
            provider.outstanding(
                &VolumeIdentity { device: 2 },
                crate::operation_journal::full_capacity::FullCapacityAllocationClass::JournalRecord,
            ),
            1
        );
        assert_eq!(provider.claim_count(), 1);
    }

    #[test]
    fn ordinary_claim_release_subtracts_one_claim_and_replays_after_token_removal() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let operation_id = OperationId::new();
        let mut record = intent_record(operation_id);
        let (volume, allocation_class) = {
            let row = &mut record
                .capacity_plan
                .as_mut()
                .expect("intent capacity plan")
                .volumes[0];
            row.logical_bytes = 2;
            row.allocated_bytes = 2;
            (row.identity.clone(), row.allocation_class.into())
        };
        let first_id = AdmissionClaimId::new();
        let second_id = AdmissionClaimId::new();
        let claims = AdmissionClaimInventory::new(vec![
            claim_for_intent(&record, first_id, Some(1)),
            claim_for_intent(&record, second_id, Some(1)),
        ])
        .expect("split claim inventory");
        let provider = provider_for_intent(&record);
        let settlement = store
            .settle_initial_claims(&record, &provider, &claims)
            .expect("settle split claims");
        drop(settlement);
        assert_eq!(provider.outstanding(&volume, allocation_class), 2);

        let ordinary = store.read_ordinary_claim_page(None, MAX_PAGE_ROWS).unwrap();
        let first = ordinary
            .records()
            .iter()
            .find(|row| row.claim_id() == first_id)
            .unwrap();
        let retained = AdmissionClaim::new(
            first.claim_id(),
            first.volume().clone(),
            first.allocation_class(),
            first.allocated_bytes(),
        )
        .unwrap();
        let publication_charge = provider
            .bind_publication_charge(first.operation_id(), &retained)
            .unwrap();
        assert!(publication_charge.is_current());

        let first_request = release_request(operation_id, first_id, Uuid::new_v4());
        store
            .release_ordinary_claim(first_request, &provider)
            .expect("release first claim");
        assert_eq!(provider.outstanding(&volume, allocation_class), 1);
        assert!(!publication_charge.is_current());
        assert!(
            provider
                .bind_publication_charge(operation_id, &retained)
                .is_err()
        );
        store
            .release_ordinary_claim(first_request, &provider)
            .expect("replay first release");
        assert_eq!(provider.outstanding(&volume, allocation_class), 1);

        let conflicting = release_request(operation_id, first_id, Uuid::new_v4());
        assert!(matches!(
            store.release_ordinary_claim(conflicting, &provider),
            Err(OrdinaryClaimReleaseError::ReleaseConflict { claim_id }) if claim_id == first_id
        ));
        assert_eq!(provider.outstanding(&volume, allocation_class), 1);

        let second_request = release_request(operation_id, second_id, Uuid::new_v4());
        store
            .release_ordinary_claim(second_request, &provider)
            .expect("release second claim");
        assert_eq!(provider.outstanding(&volume, allocation_class), 0);
        assert_eq!(provider.claim_count(), 0);
        store
            .release_ordinary_claim(first_request, &provider)
            .expect("replay after token removal");
        assert_eq!(provider.outstanding(&volume, allocation_class), 0);
        assert_eq!(ordinary_claim_count(&store), 2);
        let release_count: i64 = store
            .connection
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM {}",
                    ordinary_claims::RELEASE_TABLE_NAME
                ),
                [],
                |row| row.get(0),
            )
            .expect("count release receipts");
        assert_eq!(release_count, 2);
    }

    #[test]
    fn ordinary_release_cannot_bypass_issued_chunk_relation() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let operation_id = OperationId::new();
        let record = intent_record(operation_id);
        let claim_id = AdmissionClaimId::new();
        let claims = AdmissionClaimInventory::new(vec![claim_for_intent(&record, claim_id, None)])
            .expect("chunk-backed claim inventory");
        let provider = provider_for_intent(&record);
        drop(
            store
                .settle_initial_claims(&record, &provider, &claims)
                .expect("settle chunk-backed ordinary claim"),
        );
        let volume = record
            .capacity_plan
            .as_ref()
            .expect("capacity plan")
            .volumes[0]
            .identity
            .clone();
        let chunk_id = ChunkId::new();
        let issue_id = Uuid::new_v4();
        store
            .connection
            .execute(
                &format!(
                    "INSERT INTO {} (
                        chunk_id, operation_id, claim_id, participant_id, output_key,
                        logical_offset, logical_length, volume_device, allocation_class,
                        allocated_bytes, allocation_unit, protected_free_bytes, state,
                        issue_id, release_id
                    ) VALUES (?1, ?2, ?3, ?4, 'guarded', 0, 1, ?5, 'DestinationStaging',
                              1, 1, ?6, 'Issued', ?7, NULL)",
                    chunk_claims::TABLE_NAME
                ),
                params![
                    chunk_id.to_string(),
                    operation_id.to_string(),
                    claim_id.to_string(),
                    Uuid::from_u128(0x1234).to_string(),
                    volume.device.to_string(),
                    PROTECTED_FREE_SPACE_FLOOR as i64,
                    issue_id.to_string(),
                ],
            )
            .expect("insert issued chunk fixture");

        let request = release_request(operation_id, claim_id, Uuid::new_v4());
        assert!(matches!(
            store.release_ordinary_claim(request, &provider),
            Err(OrdinaryClaimReleaseError::ChunkClaimRequiresOwner { claim_id: actual })
                if actual == claim_id
        ));
        assert_eq!(provider.outstanding(&volume, row_class(&record)), 1);
        assert_eq!(
            store
                .connection
                .query_row(
                    &format!(
                        "SELECT state FROM {} WHERE chunk_id = ?1",
                        chunk_claims::TABLE_NAME
                    ),
                    params![chunk_id.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .expect("read guarded chunk state"),
            "Issued"
        );
    }

    #[test]
    fn chunk_claim_fault_boundaries_rollback_or_retain_exact_charge() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let rollback_operation = OperationId::new();
        let rollback_record = universal_intent_record(rollback_operation);
        let rollback_request = chunk_request_for_record(
            &rollback_record,
            ChunkId::new(),
            AdmissionClaimId::new(),
            "chunk-rollback",
            0,
        );
        store
            .insert_intent_with_outcome(&rollback_record)
            .receipt()
            .expect("insert rollback operation");
        let rollback_plan = rollback_request.plan().expect("rollback plan");
        let rollback_claims = rollback_request.inventory().expect("rollback claims");
        let rollback_provider = provider_for_intent(&rollback_record);
        let rollback_lease = rollback_provider
            .admit(&rollback_plan, &rollback_claims)
            .expect("rollback provisional lease");
        store.set_insert_test_hook(|boundary| {
            if boundary == InsertTestBoundary::BeforePrecommitValidation {
                InsertTestDirective::Fail(OperationJournalStoreError::InconsistentRecord {
                    reason: String::from("forced chunk claim rollback"),
                })
            } else {
                InsertTestDirective::Continue
            }
        });
        let rollback_error = store
            .persist_chunk_claim_with_lease(&rollback_request, &rollback_provider, rollback_lease)
            .expect_err("precommit chunk claim failure");
        assert!(matches!(
            rollback_error,
            OperationJournalStoreError::InconsistentRecord { reason }
                if reason.contains("forced chunk claim rollback")
        ));
        assert_eq!(rollback_provider.claim_count(), 0);
        assert_eq!(ordinary_claim_count(&store), 0);
        assert!(
            store
                .read_chunk_claim(rollback_request.chunk_id())
                .expect("rollback chunk read")
                .is_none()
        );

        let retained_operation = OperationId::new();
        let retained_record = universal_intent_record(retained_operation);
        let retained_request = chunk_request_for_record(
            &retained_record,
            ChunkId::new(),
            AdmissionClaimId::new(),
            "chunk-uncertain",
            0,
        );
        store
            .insert_intent_with_outcome(&retained_record)
            .receipt()
            .expect("insert uncertain operation");
        let retained_plan = retained_request.plan().expect("uncertain plan");
        let retained_claims = retained_request.inventory().expect("uncertain claims");
        let retained_provider = provider_for_intent(&retained_record);
        let retained_lease = retained_provider
            .admit(&retained_plan, &retained_claims)
            .expect("uncertain provisional lease");
        store.set_insert_test_hook(|boundary| {
            if boundary == InsertTestBoundary::Commit {
                InsertTestDirective::ExecuteThenFail(
                    OperationJournalStoreError::IdentityCheckUnavailable {
                        reason: String::from("forced chunk claim commit uncertainty"),
                    },
                )
            } else {
                InsertTestDirective::Continue
            }
        });
        let uncertain_error = store
            .persist_chunk_claim_with_lease(&retained_request, &retained_provider, retained_lease)
            .expect_err("uncertain chunk claim commit");
        assert!(matches!(
            uncertain_error,
            OperationJournalStoreError::ChunkClaim { reason }
                if reason.contains("outcome is uncertain")
        ));
        assert_eq!(retained_provider.claim_count(), 1);
        assert_eq!(ordinary_claim_count(&store), 1);
        assert_eq!(
            store
                .read_chunk_claim(retained_request.chunk_id())
                .expect("uncertain chunk read")
                .expect("retained chunk row")
                .state(),
            ChunkClaimState::Claimed
        );
    }

    #[test]
    fn chunk_issue_and_release_uncertainty_replay_without_reentry() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let operation_id = OperationId::new();
        let record = universal_intent_record(operation_id);
        let request = chunk_request_for_record(
            &record,
            ChunkId::new(),
            AdmissionClaimId::new(),
            "chunk-issue-release",
            0,
        );
        store
            .insert_intent_with_outcome(&record)
            .receipt()
            .expect("insert operation");
        let plan = request.plan().expect("chunk plan");
        let claims = request.inventory().expect("chunk claims");
        let provider = provider_for_intent(&record);
        let lease = provider.admit(&plan, &claims).expect("chunk lease");
        store
            .persist_chunk_claim_with_lease(&request, &provider, lease)
            .expect("persist chunk claim");
        let persisted = store.read_chunk_claim(request.chunk_id()).unwrap().unwrap();
        let retained = AdmissionClaim::new(
            persisted.claim_id(),
            persisted.volume().clone(),
            persisted.allocation_class(),
            persisted.allocated_bytes(),
        )
        .unwrap();
        let publication_charge = provider
            .bind_publication_charge(persisted.operation_id(), &retained)
            .unwrap();
        assert!(publication_charge.is_current());
        let issue_id = Uuid::new_v4();
        store.set_insert_test_hook(|boundary| {
            if boundary == InsertTestBoundary::Commit {
                InsertTestDirective::ExecuteThenFail(
                    OperationJournalStoreError::IdentityCheckUnavailable {
                        reason: String::from("forced issue commit uncertainty"),
                    },
                )
            } else {
                InsertTestDirective::Continue
            }
        });
        let issue_error = store
            .issue_chunk_claim(request.chunk_id(), operation_id, issue_id)
            .expect_err("uncertain issue commit");
        assert!(matches!(
            issue_error,
            OperationJournalStoreError::ChunkClaim { reason }
                if reason.contains("outcome is uncertain")
        ));
        let issued = store
            .read_chunk_claim(request.chunk_id())
            .expect("issued read")
            .expect("issued row");
        assert_eq!(issued.state(), ChunkClaimState::Issued);
        assert_eq!(issued.issue_id(), Some(issue_id));
        assert!(
            store
                .issue_chunk_claim(request.chunk_id(), operation_id, Uuid::new_v4())
                .is_err()
        );

        let release_id = Uuid::new_v4();
        let authorization = ChunkUnusedReleaseAuthorization::for_test(
            request.chunk_id(),
            operation_id,
            issue_id,
            release_id,
        );
        store.set_release_test_hook(|boundary| {
            if boundary == ReleaseTestBoundary::Commit {
                ReleaseTestDirective::ExecuteThenFail(
                    OperationJournalStoreError::IdentityCheckUnavailable {
                        reason: String::from("forced chunk release commit uncertainty"),
                    },
                )
            } else {
                ReleaseTestDirective::Continue
            }
        });
        let release_error = store
            .release_chunk_claim(
                request.chunk_id(),
                operation_id,
                Some(authorization),
                release_id,
            )
            .expect_err("uncertain release commit");
        assert!(matches!(
            release_error,
            OperationJournalStoreError::ChunkClaim { reason }
                if reason.contains("outcome is uncertain")
        ));
        let released = store
            .read_chunk_claim(request.chunk_id())
            .expect("released read")
            .expect("released row");
        assert_eq!(released.state(), ChunkClaimState::Released);
        let replay = store
            .release_chunk_claim(request.chunk_id(), operation_id, None, release_id)
            .expect("durable release replay");
        provider
            .release_claim(&replay)
            .expect("provider release after durable replay");
        assert!(!publication_charge.is_current());
        assert!(
            provider
                .bind_publication_charge(operation_id, &retained)
                .is_err()
        );
        assert_eq!(provider.claim_count(), 0);
    }

    #[test]
    fn chunk_claim_validation_rejects_oversized_relation_with_bounded_read() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let operation_id = OperationId::new();
        let operation_uuid = operation_id.as_uuid();
        let participant_id = Uuid::from_u128(0x4321);
        for index in 0..=chunk_claims::MAX_PAGE_ROWS {
            let claim_id = Uuid::from_u128(100_000 + index as u128);
            let chunk_id = Uuid::from_u128(200_000 + index as u128);
            insert_ordinary_claim(
                &store,
                claim_id,
                operation_uuid,
                "1",
                "DestinationStaging",
                1,
            )
            .expect("insert bounded ordinary claim fixture");
            store
                .connection
                .execute(
                    &format!(
                        "INSERT INTO {} (
                            chunk_id, operation_id, claim_id, participant_id, output_key,
                            logical_offset, logical_length, volume_device, allocation_class,
                            allocated_bytes, allocation_unit, protected_free_bytes, state,
                            issue_id, release_id
                        ) VALUES (?1, ?2, ?3, ?4, 'bounded', 0, 1, '1',
                                  'DestinationStaging', 1, 1, ?5, 'Claimed', NULL, NULL)",
                        chunk_claims::TABLE_NAME
                    ),
                    params![
                        chunk_id.to_string(),
                        operation_id.to_string(),
                        claim_id.to_string(),
                        participant_id.to_string(),
                        PROTECTED_FREE_SPACE_FLOOR as i64,
                    ],
                )
                .expect("insert bounded chunk fixture");
        }
        let error = store
            .validate_chunk_claims_inside_transaction()
            .expect_err("oversized chunk relation must fail closed");
        assert!(matches!(
            error,
            OperationJournalStoreError::ChunkClaim { reason }
                if reason.contains("bounded") && reason.contains("chunk-claim")
        ));
    }

    #[test]
    fn chunk_claim_append_enforces_global_bound_before_side_effects() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let operation_id = OperationId::new();
        let record = universal_intent_record(operation_id);
        store
            .insert_intent_with_outcome(&record)
            .receipt()
            .expect("insert first operation");
        let participant_id = record
            .universal_intent
            .as_ref()
            .expect("universal intent")
            .participants()
            .descriptors()[0]
            .id();
        for index in 0..chunk_claims::MAX_PAGE_ROWS - 1 {
            let claim_id = Uuid::from_u128(300_000 + index as u128);
            let chunk_id = Uuid::from_u128(400_000 + index as u128);
            insert_ordinary_claim(
                &store,
                claim_id,
                operation_id.as_uuid(),
                "1",
                "DestinationStaging",
                1,
            )
            .expect("insert at-limit ordinary fixture");
            store
                .connection
                .execute(
                    &format!(
                        "INSERT INTO {} (
                            chunk_id, operation_id, claim_id, participant_id, output_key,
                            logical_offset, logical_length, volume_device, allocation_class,
                            allocated_bytes, allocation_unit, protected_free_bytes, state,
                            issue_id, release_id
                        ) VALUES (?1, ?2, ?3, ?4, 'at-limit', 0, 1, '1',
                                  'DestinationStaging', 1, 1, ?5, 'Claimed', NULL, NULL)",
                        chunk_claims::TABLE_NAME
                    ),
                    params![
                        chunk_id.to_string(),
                        operation_id.to_string(),
                        claim_id.to_string(),
                        participant_id.to_string(),
                        PROTECTED_FREE_SPACE_FLOOR as i64,
                    ],
                )
                .expect("insert at-limit chunk fixture");
        }

        let first_request = chunk_request_for_record(
            &record,
            ChunkId::new(),
            AdmissionClaimId::new(),
            "at-limit-api",
            0,
        );
        let first_plan = first_request.plan().expect("first plan");
        let first_claims = first_request.inventory().expect("first claims");
        let first_provider = provider_for_intent(&record);
        let first_lease = first_provider
            .admit(&first_plan, &first_claims)
            .expect("first append lease");
        store
            .persist_chunk_claim_with_lease(&first_request, &first_provider, first_lease)
            .expect("append exactly at global bound");
        assert_eq!(ordinary_claim_count(&store), chunk_claims::MAX_PAGE_ROWS);
        let chunk_count: i64 = store
            .connection
            .query_row(
                &format!("SELECT COUNT(*) FROM {}", chunk_claims::TABLE_NAME),
                [],
                |row| row.get(0),
            )
            .expect("count at-limit chunk rows");
        assert_eq!(chunk_count, chunk_claims::MAX_PAGE_ROWS as i64);
        assert_eq!(first_provider.claim_count(), 1);

        let second_operation_id = OperationId::new();
        let second_record = universal_intent_record(second_operation_id);
        store
            .insert_intent_with_outcome(&second_record)
            .receipt()
            .expect("insert second operation");
        let second_request = chunk_request_for_record(
            &second_record,
            ChunkId::new(),
            AdmissionClaimId::new(),
            "over-global-limit",
            0,
        );
        let second_plan = second_request.plan().expect("second plan");
        let second_claims = second_request.inventory().expect("second claims");
        let second_provider = provider_for_intent(&second_record);
        let second_lease = second_provider
            .admit(&second_plan, &second_claims)
            .expect("second append lease");
        let error = store
            .persist_chunk_claim_with_lease(&second_request, &second_provider, second_lease)
            .expect_err("global chunk bound must reject second operation");
        assert!(matches!(
            error,
            OperationJournalStoreError::ChunkClaim { reason }
                if reason.contains("bounded") && reason.contains("chunk-claim")
        ));
        assert_eq!(ordinary_claim_count(&store), chunk_claims::MAX_PAGE_ROWS);
        let chunk_count_after: i64 = store
            .connection
            .query_row(
                &format!("SELECT COUNT(*) FROM {}", chunk_claims::TABLE_NAME),
                [],
                |row| row.get(0),
            )
            .expect("count preserved chunk rows");
        assert_eq!(chunk_count_after, chunk_claims::MAX_PAGE_ROWS as i64);
        assert_eq!(second_provider.claim_count(), 0);
        drop(fixture);
    }

    #[test]
    fn ordinary_claim_release_retains_charge_when_provider_reconciliation_fails() {
        use crate::operation_journal::capacity_retention_telemetry::{
            CapacityLedgerCall as K, CapacityLedgerReturn as R, tests::Capture,
        };
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let operation_id = OperationId::new();
        let record = intent_record(operation_id);
        let claims = claims_for_intent(&record);
        let claim_id = claims.claims()[0].claim_id();
        let provider = provider_for_intent(&record);
        let settlement = store
            .settle_initial_claims(&record, &provider, &claims)
            .expect("settle claim");
        drop(settlement);
        let capture = Capture::new();
        let request = release_request(operation_id, claim_id, Uuid::new_v4());
        provider.fail_release_for_test();
        assert!(matches!(
            store.release_ordinary_claim(request, &provider),
            Err(OrdinaryClaimReleaseError::ProviderRetained { .. })
        ));
        assert_eq!(
            capture
                .snapshot()
                .count(K::OrdinaryClaimRelease, R::Refused),
            1
        );
        assert_eq!(
            capture
                .snapshot()
                .count(K::OrdinaryClaimRelease, R::Confirmed),
            0
        );
        let row = &record
            .capacity_plan
            .as_ref()
            .expect("capacity plan")
            .volumes[0];
        assert_eq!(
            provider.outstanding(&row.identity, row.allocation_class.into()),
            1
        );
        provider.allow_release_for_test();
        store
            .release_ordinary_claim(request, &provider)
            .expect("retry committed release");
        assert_eq!(
            provider.outstanding(&row.identity, row.allocation_class.into()),
            0
        );
        let snapshot = capture.snapshot();
        assert_eq!(snapshot.count(K::OrdinaryClaimRelease, R::Refused), 1);
        assert_eq!(snapshot.count(K::OrdinaryClaimRelease, R::Confirmed), 1);
        assert_eq!(snapshot.elapsed_buckets.iter().sum::<u64>(), 2);
    }

    #[test]
    fn ordinary_claim_release_replays_after_journal_reopen() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let operation_id = OperationId::new();
        let record = intent_record(operation_id);
        let claims = claims_for_intent(&record);
        let claim_id = claims.claims()[0].claim_id();
        let provider = provider_for_intent(&record);
        drop(
            store
                .settle_initial_claims(&record, &provider, &claims)
                .expect("settle claim"),
        );
        let request = release_request(operation_id, claim_id, Uuid::new_v4());
        store
            .release_ordinary_claim(request, &provider)
            .expect("release claim");
        drop(store);
        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen store");
        reopened
            .release_ordinary_claim(request, &provider)
            .expect("replay durable release after reopen");
        assert_eq!(provider.claim_count(), 0);
    }

    #[test]
    fn ordinary_claim_rebuild_reopens_and_preserves_partial_release_identity() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let operation_id = OperationId::new();
        let mut record = intent_record(operation_id);
        record
            .capacity_plan
            .as_mut()
            .expect("intent capacity plan")
            .volumes[0]
            .logical_bytes = 2;
        record
            .capacity_plan
            .as_mut()
            .expect("intent capacity plan")
            .volumes[0]
            .allocated_bytes = 2;
        let first_id = AdmissionClaimId::new();
        let second_id = AdmissionClaimId::new();
        let claims = AdmissionClaimInventory::new(vec![
            claim_for_intent(&record, first_id, Some(1)),
            claim_for_intent(&record, second_id, Some(1)),
        ])
        .expect("split claim inventory");
        let provider = provider_for_intent(&record);
        drop(
            store
                .settle_initial_claims(&record, &provider, &claims)
                .expect("settle split claims"),
        );

        let snapshot = store
            .rebuild_ordinary_claim_snapshot()
            .expect("read initial rebuild snapshot");
        assert_eq!(snapshot.claims().len(), 2);
        assert_eq!(snapshot.readiness(), &CapacityRebuildReadiness::Ready);
        let rebuilt = CapacityProvider::unavailable();
        rebuilt
            .install_rebuilt_claims(&snapshot)
            .expect("install initial rebuild snapshot");
        let volume = VolumeIdentity { device: 1 };
        let class = crate::operation_journal::full_capacity::FullCapacityAllocationClass::DestinationStaging;
        assert_eq!(rebuilt.outstanding(&volume, class), 2);

        let request = release_request(operation_id, first_id, Uuid::new_v4());
        store
            .release_ordinary_claim(request, &provider)
            .expect("release first claim");
        assert_eq!(provider.outstanding(&volume, class), 1);
        drop(store);

        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen store");
        let released_snapshot = reopened
            .rebuild_ordinary_claim_snapshot()
            .expect("read release-aware rebuild snapshot");
        assert_eq!(released_snapshot.claims().len(), 2);
        assert_eq!(
            released_snapshot.readiness(),
            &CapacityRebuildReadiness::Ready
        );
        assert_eq!(
            released_snapshot
                .claims()
                .iter()
                .filter(|claim| claim.release_id().is_some())
                .count(),
            1
        );
        let rebuilt_after_release = CapacityProvider::unavailable();
        rebuilt_after_release
            .install_rebuilt_claims(&released_snapshot)
            .expect("install release-aware rebuild snapshot");
        assert_eq!(rebuilt_after_release.outstanding(&volume, class), 1);
        rebuilt_after_release
            .install_rebuilt_claims(&released_snapshot)
            .expect("replay release-aware rebuild snapshot");
        assert_eq!(rebuilt_after_release.outstanding(&volume, class), 1);

        assert_eq!(ordinary_claim_count(&reopened), 2);
        let release_count: i64 = reopened
            .connection
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM {}",
                    ordinary_claims::RELEASE_TABLE_NAME
                ),
                [],
                |row| row.get(0),
            )
            .expect("count release evidence");
        assert_eq!(release_count, 1);
    }

    #[test]
    fn ordinary_claim_release_transactions_serialize_same_claim_and_preserve_conflict() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let claim_id = AdmissionClaimId::new();
        let operation_id = OperationId::new();
        insert_ordinary_claim(
            &store,
            claim_id.as_uuid(),
            operation_id.as_uuid(),
            "1",
            "DestinationStaging",
            1,
        )
        .expect("insert claim fixture");
        let path = database_path(&fixture.owner);
        drop(store);

        let release_id = Uuid::new_v4();
        let first_connection = Connection::open(&path).expect("open first release connection");
        let second_connection = Connection::open(&path).expect("open second release connection");
        first_connection
            .busy_timeout(BUSY_TIMEOUT)
            .expect("first busy timeout");
        second_connection
            .busy_timeout(BUSY_TIMEOUT)
            .expect("second busy timeout");
        first_connection
            .execute_batch("PRAGMA foreign_keys = ON")
            .expect("first foreign keys");
        second_connection
            .execute_batch("PRAGMA foreign_keys = ON")
            .expect("second foreign keys");
        let (ready_tx, ready_rx) = mpsc::channel();
        let (blocked_tx, blocked_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let first_thread = thread::spawn(move || {
            first_connection
                .execute_batch("BEGIN EXCLUSIVE")
                .expect("first begin exclusive");
            assert!(matches!(
                release_ordinary_claim_transition_inside_transaction(
                    &first_connection,
                    release_request(operation_id, claim_id, release_id),
                )
                .expect("first insert release"),
                OrdinaryClaimReleaseTransition::Inserted(receipt)
                    if receipt.release_id() == release_id
            ));
            ready_tx.send(()).expect("signal first transaction");
            release_rx.recv().expect("wait to commit first transaction");
            first_connection
                .execute_batch("COMMIT")
                .expect("commit first release");
        });
        let second_thread = thread::spawn(move || {
            ready_rx.recv().expect("wait for first transaction");
            blocked_tx.send(()).expect("signal second begin");
            second_connection
                .execute_batch("BEGIN EXCLUSIVE")
                .expect("second begin waits for first");
            assert!(matches!(
                release_ordinary_claim_transition_inside_transaction(
                    &second_connection,
                    release_request(operation_id, claim_id, release_id),
                )
                .expect("read serialized release"),
                OrdinaryClaimReleaseTransition::Replay(receipt)
                    if receipt.release_id() == release_id
            ));
            second_connection
                .execute_batch("COMMIT")
                .expect("commit serialized read");
        });
        blocked_rx
            .recv()
            .expect("second transaction attempted begin");
        release_tx.send(()).expect("release first transaction");
        first_thread.join().expect("first release thread");
        second_thread.join().expect("second release thread");

        let connection = Connection::open(&path).expect("open conflict check connection");
        connection
            .execute_batch("BEGIN EXCLUSIVE")
            .expect("begin conflict check");
        let conflicting = release_request(operation_id, claim_id, Uuid::new_v4());
        assert!(matches!(
            release_ordinary_claim_transition_inside_transaction(&connection, conflicting),
            Err(OrdinaryClaimReleaseError::ReleaseConflict { claim_id: id }) if id == claim_id
        ));
        connection
            .execute_batch("ROLLBACK")
            .expect("rollback conflict check");
    }

    #[test]
    fn ordinary_claim_release_retries_post_commit_uncertainty_and_rollback_uncertainty() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let operation_id = OperationId::new();
        let record = intent_record(operation_id);
        let claims = claims_for_intent(&record);
        let claim_id = claims.claims()[0].claim_id();
        let provider = provider_for_intent(&record);
        drop(
            store
                .settle_initial_claims(&record, &provider, &claims)
                .expect("settle claim"),
        );
        let request = release_request(operation_id, claim_id, Uuid::new_v4());
        store.set_release_test_hook(|boundary| {
            if boundary == ReleaseTestBoundary::AfterCommit {
                ReleaseTestDirective::Fail(OperationJournalStoreError::InconsistentRecord {
                    reason: String::from("injected post-commit acknowledgement failure"),
                })
            } else {
                ReleaseTestDirective::Continue
            }
        });
        assert!(matches!(
            store.release_ordinary_claim(request, &provider),
            Err(OrdinaryClaimReleaseError::PersistenceUncertain { .. })
        ));
        assert_eq!(
            provider.outstanding(&VolumeIdentity { device: 1 }, row_class(&record)),
            1
        );
        drop(store);
        let reopened =
            OperationJournalStore::open(&fixture.owner).expect("reopen postcommit release");
        let snapshot = reopened
            .rebuild_ordinary_claim_snapshot()
            .expect("rebuild after postcommit release uncertainty");
        assert_eq!(snapshot.claims().len(), 1);
        assert!(snapshot.claims()[0].release_id().is_some());
        let rebuilt = CapacityProvider::unavailable();
        rebuilt
            .install_rebuilt_claims(&snapshot)
            .expect("install postcommit release snapshot");
        assert_eq!(rebuilt.claim_count(), 0);
        reopened
            .release_ordinary_claim(request, &provider)
            .expect("retry post-commit release");
        assert_eq!(
            provider.outstanding(&VolumeIdentity { device: 1 }, row_class(&record)),
            0
        );

        let store = reopened;
        let operation_id = OperationId::new();
        let record = intent_record(operation_id);
        let claims = claims_for_intent(&record);
        let claim_id = claims.claims()[0].claim_id();
        let provider = provider_for_intent(&record);
        drop(
            store
                .settle_initial_claims(&record, &provider, &claims)
                .expect("settle second claim"),
        );
        let request = release_request(operation_id, claim_id, Uuid::new_v4());
        store.set_release_test_hook(|boundary| match boundary {
            ReleaseTestBoundary::BeforePrecommitValidation => {
                ReleaseTestDirective::FailAndLeaveForRollback(
                    OperationJournalStoreError::InconsistentRecord {
                        reason: String::from("injected pre-commit failure"),
                    },
                )
            }
            ReleaseTestBoundary::Rollback => ReleaseTestDirective::ExecuteThenFail(
                OperationJournalStoreError::InconsistentRecord {
                    reason: String::from("injected rollback uncertainty"),
                },
            ),
            _ => ReleaseTestDirective::Continue,
        });
        assert!(matches!(
            store.release_ordinary_claim(request, &provider),
            Err(OrdinaryClaimReleaseError::PersistenceUncertain { .. })
        ));
        drop(store);
        let reopened =
            OperationJournalStore::open(&fixture.owner).expect("reopen rollback-uncertain release");
        let snapshot = reopened
            .rebuild_ordinary_claim_snapshot()
            .expect("rebuild after rollback release uncertainty");
        assert_eq!(snapshot.claims().len(), 2);
        assert!(
            snapshot
                .claims()
                .iter()
                .find(|claim| claim.operation_id() == record.operation_id)
                .expect("find rollback-uncertain claim")
                .release_id()
                .is_none()
        );
        let rebuilt = CapacityProvider::unavailable();
        rebuilt
            .install_rebuilt_claims(&snapshot)
            .expect("install rollback-uncertain release snapshot");
        assert_eq!(
            rebuilt.outstanding(&VolumeIdentity { device: 1 }, row_class(&record)),
            1
        );
        reopened
            .release_ordinary_claim(request, &provider)
            .expect("retry rollback uncertainty");
        assert_eq!(
            provider.outstanding(&VolumeIdentity { device: 1 }, row_class(&record)),
            0
        );
    }

    fn row_class(
        record: &StoredOperationRecord,
    ) -> crate::operation_journal::full_capacity::FullCapacityAllocationClass {
        record
            .capacity_plan
            .as_ref()
            .expect("capacity plan")
            .volumes[0]
            .allocation_class
            .into()
    }

    #[test]
    fn confirmed_rollback_removes_intent_and_claim_rows_and_releases_charge() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        store.set_insert_test_hook(|boundary| {
            if boundary == InsertTestBoundary::BeforePrecommitValidation {
                InsertTestDirective::Fail(OperationJournalStoreError::IdentityCheckUnavailable {
                    reason: String::from("forced claim rollback"),
                })
            } else {
                InsertTestDirective::Continue
            }
        });
        let record = intent_record(OperationId::new());
        let claims = claims_for_intent(&record);
        let provider = provider_for_intent(&record);

        let settlement = store
            .settle_initial_claims(&record, &provider, &claims)
            .expect("settlement reports rollback outcome");
        let failure = settlement.outcome().failure().expect("rollback failure");
        assert_eq!(
            failure.stage(),
            JournalInsertWriteStage::RolledBackBeforeCommit
        );
        assert!(settlement.lease().is_none());
        assert_eq!(store.get(record.operation_id).expect("read rollback"), None);
        assert_eq!(ordinary_claim_count(&store), 0);
        assert_eq!(provider.claim_count(), 0);

        drop(store);
        let reopened =
            OperationJournalStore::open(&fixture.owner).expect("reopen rolled-back journal");
        let snapshot = reopened
            .rebuild_ordinary_claim_snapshot()
            .expect("rebuild after confirmed rollback");
        assert!(snapshot.claims().is_empty());
        assert!(matches!(
            snapshot.readiness(),
            CapacityRebuildReadiness::RecoveryOnly { .. }
        ));
    }

    #[test]
    fn rollback_not_confirmed_keeps_claim_charge_pinned() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        store.set_insert_test_hook(|boundary| match boundary {
            InsertTestBoundary::BeforePrecommitValidation | InsertTestBoundary::Rollback => {
                InsertTestDirective::FailAndLeaveForRollback(
                    OperationJournalStoreError::IdentityCheckUnavailable {
                        reason: String::from("forced rollback uncertainty"),
                    },
                )
            }
            _ => InsertTestDirective::Continue,
        });
        let record = intent_record(OperationId::new());
        let claims = claims_for_intent(&record);
        let provider = provider_for_intent(&record);
        let settlement = store
            .settle_initial_claims(&record, &provider, &claims)
            .expect("uncertain rollback outcome");
        assert!(matches!(
            settlement
                .outcome()
                .failure()
                .map(JournalInsertFailure::stage),
            Some(JournalInsertWriteStage::RollbackNotConfirmed)
        ));
        assert!(matches!(
            settlement.lease().map(|lease| lease.disposition()),
            Some(ProvisionalLeaseDisposition::AuditPending { .. })
        ));
        drop(settlement);
        assert_eq!(provider.claim_count(), 1);

        store.set_insert_test_hook(|_| InsertTestDirective::Continue);
        store
            .connection
            .execute_batch("ROLLBACK")
            .expect("clean up uncertain test transaction");
        assert_eq!(
            store.get(record.operation_id).expect("read after cleanup"),
            None
        );
        assert_eq!(ordinary_claim_count(&store), 0);
    }

    #[test]
    fn ambiguous_commit_and_postcommit_readback_retain_charge_after_lease_drop() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let first = intent_record(OperationId::new());
        let first_claims = claims_for_intent(&first);
        let provider = provider_for_intent(&first);
        store.set_insert_test_hook(|boundary| {
            if boundary == InsertTestBoundary::Commit {
                InsertTestDirective::ExecuteThenFail(
                    OperationJournalStoreError::IdentityCheckUnavailable {
                        reason: String::from("forced ambiguous commit"),
                    },
                )
            } else {
                InsertTestDirective::Continue
            }
        });
        let first_settlement = store
            .settle_initial_claims(&first, &provider, &first_claims)
            .expect("ambiguous commit outcome");
        assert!(matches!(
            first_settlement
                .outcome()
                .failure()
                .map(JournalInsertFailure::stage),
            Some(JournalInsertWriteStage::CommitOutcomeUnknown)
        ));
        assert!(matches!(
            first_settlement.lease().map(|lease| lease.disposition()),
            Some(ProvisionalLeaseDisposition::AuditPending { .. })
        ));
        drop(first_settlement);
        assert_eq!(provider.claim_count(), 1);

        let second = intent_record(OperationId::new());
        let second_claims = claims_for_intent(&second);
        store.set_insert_test_hook(|boundary| {
            if boundary == InsertTestBoundary::AfterCommit {
                InsertTestDirective::Fail(OperationJournalStoreError::IdentityCheckUnavailable {
                    reason: String::from("forced readback failure"),
                })
            } else {
                InsertTestDirective::Continue
            }
        });
        let second_settlement = store
            .settle_initial_claims(&second, &provider, &second_claims)
            .expect("postcommit outcome");
        assert!(matches!(
            second_settlement
                .outcome()
                .failure()
                .map(JournalInsertFailure::stage),
            Some(JournalInsertWriteStage::CommittedButNotAcknowledged)
        ));
        assert!(matches!(
            second_settlement.lease().map(|lease| lease.disposition()),
            Some(ProvisionalLeaseDisposition::AuditPending { .. })
        ));
        drop(second_settlement);
        assert_eq!(provider.claim_count(), 2);

        drop(store);
        let reopened =
            OperationJournalStore::open(&fixture.owner).expect("reopen uncertain journal");
        let snapshot = reopened
            .rebuild_ordinary_claim_snapshot()
            .expect("rebuild after uncertain initial settlements");
        assert_eq!(snapshot.claims().len(), 2);
        assert_eq!(snapshot.readiness(), &CapacityRebuildReadiness::Ready);
        let rebuilt = CapacityProvider::unavailable();
        rebuilt
            .install_rebuilt_claims(&snapshot)
            .expect("install claims after uncertain initial settlements");
        assert_eq!(
            rebuilt.outstanding(&VolumeIdentity { device: 1 }, row_class(&first)),
            2
        );
    }

    #[test]
    fn duplicate_operation_and_claim_collision_release_only_new_provisional_charge() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let first = intent_record(OperationId::new());
        let first_claim = claim_for_intent(&first, AdmissionClaimId::new(), None);
        let first_claims =
            AdmissionClaimInventory::new(vec![first_claim.clone()]).expect("first claim inventory");
        let provider = provider_for_intent(&first);
        let first_settlement = store
            .settle_initial_claims(&first, &provider, &first_claims)
            .expect("first settlement");
        drop(first_settlement);
        assert_eq!(provider.claim_count(), 1);

        let duplicate = store
            .settle_initial_claims(&first, &provider, &first_claims)
            .expect("duplicate is a typed failed outcome");
        assert!(matches!(
            duplicate.outcome().failure().map(JournalInsertFailure::error),
            Some(OperationJournalStoreError::DuplicateOperation { operation_id })
                if *operation_id == first.operation_id
        ));
        assert!(duplicate.lease().is_none());
        assert_eq!(provider.claim_count(), 1);

        let second = intent_record(OperationId::new());
        let collision_claims =
            AdmissionClaimInventory::new(vec![first_claim]).expect("collision claim inventory");
        let collision = store
            .settle_initial_claims(&second, &provider, &collision_claims)
            .expect("claim collision is a typed failed outcome");
        assert!(collision.outcome().failure().is_some());
        assert!(collision.lease().is_none());
        assert_eq!(
            store.get(second.operation_id).expect("read collision"),
            None
        );
        assert_eq!(ordinary_claim_count(&store), 1);
        assert_eq!(provider.claim_count(), 1);
    }

    #[test]
    fn failed_confirmed_rollback_returns_outcome_and_retained_lease() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        store.set_insert_test_hook(|boundary| {
            if boundary == InsertTestBoundary::BeforePrecommitValidation {
                InsertTestDirective::Fail(OperationJournalStoreError::IdentityCheckUnavailable {
                    reason: String::from("forced release failure"),
                })
            } else {
                InsertTestDirective::Continue
            }
        });
        let record = intent_record(OperationId::new());
        let claims = claims_for_intent(&record);
        let expected_claim_ids = claims
            .claims()
            .iter()
            .map(AdmissionClaim::claim_id)
            .collect::<Vec<_>>();
        let provider = provider_for_intent(&record);
        provider.fail_release_for_test();

        let result = store.settle_initial_claims(&record, &provider, &claims);
        match result {
            Err(InitialClaimSettlementError::OutcomeRetained {
                outcome,
                lease,
                source,
            }) => {
                assert!(matches!(
                    outcome.failure().map(JournalInsertFailure::stage),
                    Some(JournalInsertWriteStage::RolledBackBeforeCommit)
                ));
                assert_eq!(source, CapacityProviderError::ReleaseFailed);
                assert!(matches!(
                    lease.disposition(),
                    ProvisionalLeaseDisposition::AuditPending {
                        operation_id,
                        claim_ids,
                    } if *operation_id == record.operation_id && claim_ids == &expected_claim_ids
                ));
                drop(lease);
            }
            other => panic!("unexpected settlement result: {other:?}"),
        }
        assert_eq!(provider.claim_count(), 1);
        assert_eq!(
            provider.ledger_dispositions(),
            vec![ProvisionalLeaseDisposition::AuditPending {
                operation_id: record.operation_id,
                claim_ids: expected_claim_ids,
            }]
        );
        assert_eq!(ordinary_claim_count(&store), 0);
    }

    #[test]
    fn durable_binding_failure_returns_committed_outcome_and_retained_audit_identity() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let record = intent_record(OperationId::new());
        let claims = claims_for_intent(&record);
        let expected_claim_ids = claims
            .claims()
            .iter()
            .map(AdmissionClaim::claim_id)
            .collect::<Vec<_>>();
        let provider = provider_for_intent(&record);
        provider.fail_disposition_for_test();

        let result = store.settle_initial_claims(&record, &provider, &claims);
        match result {
            Err(InitialClaimSettlementError::OutcomeRetained {
                outcome,
                lease,
                source,
            }) => {
                assert!(outcome.receipt().is_some());
                assert_eq!(source, CapacityProviderError::LedgerBusy);
                assert!(matches!(
                    lease.disposition(),
                    ProvisionalLeaseDisposition::AuditPending {
                        operation_id,
                        claim_ids,
                    } if *operation_id == record.operation_id && claim_ids == &expected_claim_ids
                ));
                drop(lease);
            }
            other => panic!("unexpected settlement result: {other:?}"),
        }
        provider.allow_disposition_for_test();
        assert_eq!(
            provider.ledger_dispositions(),
            vec![ProvisionalLeaseDisposition::AuditPending {
                operation_id: record.operation_id,
                claim_ids: expected_claim_ids,
            }]
        );
        assert_eq!(
            store
                .get(record.operation_id)
                .expect("read committed record"),
            Some(record)
        );
        assert_eq!(ordinary_claim_count(&store), 1);
    }

    #[test]
    fn invalid_claims_and_storage_range_fail_before_provider_or_sqlite_mutation() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");

        let under_record = {
            let mut record = intent_record(OperationId::new());
            let row = &mut record.capacity_plan.as_mut().expect("plan").volumes[0];
            row.logical_bytes = 2;
            row.allocated_bytes = 2;
            record
        };
        let under_claim = claim_for_intent(&under_record, AdmissionClaimId::new(), Some(1));
        let under_claims = AdmissionClaimInventory::new(vec![under_claim]).expect("under claims");
        let under_provider = CapacityProvider::unavailable();
        assert!(matches!(
            store.settle_initial_claims(&under_record, &under_provider, &under_claims),
            Err(InitialClaimSettlementError::Claims(_))
        ));
        assert_eq!(under_provider.claim_count(), 0);

        let over_record = intent_record(OperationId::new());
        let over_claim = claim_for_intent(&over_record, AdmissionClaimId::new(), Some(2));
        let over_claims = AdmissionClaimInventory::new(vec![over_claim]).expect("over claims");
        let over_provider = CapacityProvider::unavailable();
        assert!(matches!(
            store.settle_initial_claims(&over_record, &over_provider, &over_claims),
            Err(InitialClaimSettlementError::Claims(_))
        ));
        assert_eq!(over_provider.claim_count(), 0);

        let duplicate_record = intent_record(OperationId::new());
        let duplicate_claim = claim_for_intent(&duplicate_record, AdmissionClaimId::new(), None);
        let duplicate_claims =
            AdmissionClaimInventory::new(vec![duplicate_claim.clone(), duplicate_claim]);
        assert!(matches!(
            duplicate_claims,
            Err(crate::operation_journal::admission_claims::AdmissionClaimValidationError::DuplicateClaimId { .. })
        ));

        let unavailable_record = intent_record(OperationId::new());
        let unavailable_claims = claims_for_intent(&unavailable_record);
        let unavailable_provider = CapacityProvider::unavailable();
        assert!(matches!(
            store.settle_initial_claims(
                &unavailable_record,
                &unavailable_provider,
                &unavailable_claims,
            ),
            Err(InitialClaimSettlementError::Capacity(
                CapacityProviderError::Unavailable
            ))
        ));
        assert_eq!(unavailable_provider.claim_count(), 0);

        let mut low_record = intent_record(OperationId::new());
        let low_row = &mut low_record.capacity_plan.as_mut().expect("plan").volumes[0];
        low_row.logical_bytes = 2;
        low_row.allocated_bytes = 2;
        let low_claims = claims_for_intent(&low_record);
        let mut low_facts = BTreeMap::new();
        low_facts.insert(
            VolumeIdentity { device: 1 },
            VolumeFacts {
                identity: VolumeIdentity { device: 1 },
                free_bytes: PROTECTED_FREE_SPACE_FLOOR + 1,
                allocation_unit: 1,
            },
        );
        let low_provider = CapacityProvider::qualified_fixture(low_facts);
        assert!(matches!(
            store.settle_initial_claims(&low_record, &low_provider, &low_claims),
            Err(InitialClaimSettlementError::Capacity(
                CapacityProviderError::DiskPressureRecoveryOnly { .. }
            ))
        ));
        assert_eq!(low_provider.claim_count(), 0);

        let mut huge_record = intent_record(OperationId::new());
        let huge = i64::MAX as u64 + 1;
        let huge_row = &mut huge_record.capacity_plan.as_mut().expect("plan").volumes[0];
        huge_row.logical_bytes = huge;
        huge_row.allocated_bytes = huge;
        let huge_claims = claims_for_intent(&huge_record);
        let huge_provider = CapacityProvider::unavailable();
        assert!(matches!(
            store.settle_initial_claims(&huge_record, &huge_provider, &huge_claims),
            Err(InitialClaimSettlementError::ClaimStorage(_))
        ));
        assert_eq!(huge_provider.claim_count(), 0);
        assert_eq!(ordinary_claim_count(&store), 0);
        assert_eq!(
            store.get(under_record.operation_id).expect("under absent"),
            None
        );
        assert_eq!(
            store.get(over_record.operation_id).expect("over absent"),
            None
        );
        assert_eq!(
            store.get(huge_record.operation_id).expect("huge absent"),
            None
        );
    }
}
